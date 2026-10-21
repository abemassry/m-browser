//! User-mode networking for QEMU (`-nic user`), provided by the host.
//!
//! QEMU's WASI build has no libslirp. Its `user` network backend
//! (net/wasi-user.c.inc) instead passes every Ethernet frame the guest sends
//! to `env.host_net_send`, and fetches frames for the guest from
//! `env.host_net_recv`. This module is the network at the other end of that
//! cable. It uses the same addresses as QEMU's own user networking:
//!
//! | address   | what                                                  |
//! |-----------|-------------------------------------------------------|
//! | 10.0.2.15 | the guest, handed out by DHCP                         |
//! | 10.0.2.2  | gateway; its ports are the host's own 127.0.0.1 ports |
//! | 10.0.2.3  | DNS server, forwarding to the host's resolver         |
//!
//! What it does:
//! - ARP for 10.0.2.2 and 10.0.2.3, and DHCP.
//! - TCP, relayed through host TCP sockets (see the `tcp` module), and port
//!   forwarding from the host into the guest (QEMU's hostfwd option).
//! - UDP, including DNS, relayed through host UDP sockets.
//! - Pings. 10.0.2.2 and 10.0.2.3 answer directly; other addresses are
//!   pinged from the host with an unprivileged ICMP socket. Linux allows
//!   those only if the user's group is in net.ipv4.ping_group_range, as for
//!   the ping command.
//! - IP fragmentation, both ways.
//!
//! There is no IPv6: the guest only has a link-local IPv6 address, so its
//! programs use IPv4.
//!
//! Frames are handled on a thread of their own, so that waiting on host
//! sockets never holds up the emulation.

mod tcp;

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Registry, Token, Waker};
use wasmtime::{Caller, Linker};

use crate::qemu_jit::QemuHostState;

const GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 15);
const GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 2);
const DNS_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 3);
const NETMASK: Ipv4Addr = Ipv4Addr::new(255, 255, 255, 0);
const BROADCAST_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 2, 255);
/// Hardware address of the gateway and the DNS server (as in QEMU's slirp).
const GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 10, 0, 2, 2];
const DHCP_LEASE_SECS: u32 = 24 * 60 * 60;

/// Largest IP packet sent to the guest; longer ones are fragmented.
const MTU: usize = 1500;
/// Frames waiting to cross in either direction are capped, as in a NIC's
/// queue: when the other side stops taking them, new ones are dropped.
const QUEUE_MAX: usize = 1024;
/// Host sockets are closed after this long without traffic.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// DNS queries each come from a new port, so their sockets go sooner.
const DNS_IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Packets arriving in fragments are reassembled, up to this many at once;
/// one still incomplete after `REASSEMBLY_TIMEOUT` is dropped.
const MAX_REASSEMBLIES: usize = 64;
const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(30);

const ETH_ARP: u16 = 0x0806;
const ETH_IPV4: u16 = 0x0800;
const IP_ICMP: u8 = 1;
const IP_TCP: u8 = 6;
const IP_UDP: u8 = 17;

const WAKE: Token = Token(0);

/// Add QEMU's user network imports, `env.host_net_send`, `host_net_recv`,
/// `host_net_wait` and `host_net_hostfwd` (see net/wasi-user.c.inc in QEMU),
/// backed by a new network that lives as long as the linker.
pub fn add_net_to_linker(linker: &mut Linker<QemuHostState>) -> anyhow::Result<()> {
    let net = Arc::new(VirtualNet::start()?);

    let n = net.clone();
    linker.func_wrap(
        "env",
        "host_net_send",
        move |mut caller: Caller<'_, QemuHostState>, ptr: u32, len: u32| -> i32 {
            let Some(memory) = guest_memory(&mut caller) else {
                return -1;
            };
            let start = ptr as usize;
            match memory.data(&caller).get(start..start.saturating_add(len as usize)) {
                Some(frame) => {
                    n.from_guest(frame);
                    len as i32
                }
                None => -1,
            }
        },
    )?;

    let n = net.clone();
    linker.func_wrap(
        "env",
        "host_net_recv",
        move |mut caller: Caller<'_, QemuHostState>, ptr: u32, size: u32| -> i32 {
            // Called on every main loop iteration: return quickly when idle.
            let Some(frame) = n.to_guest() else {
                return 0;
            };
            let Some(memory) = guest_memory(&mut caller) else {
                return 0;
            };
            if frame.len() > size as usize {
                return 0;
            }
            let start = ptr as usize;
            match memory.data_mut(&mut caller).get_mut(start..start + frame.len()) {
                Some(buf) => {
                    buf.copy_from_slice(&frame);
                    frame.len() as i32
                }
                None => 0,
            }
        },
    )?;

    // QEMU sleeps in this while the guest is idle (see wasi_idle_wait).
    let n = net.clone();
    linker.func_wrap("env", "host_net_wait", move |timeout_ns: i64| {
        let _timer = crate::qemu_profile::time_host(crate::qemu_profile::HostWork::IdleWait);
        n.wait_for_frame(Duration::from_nanos(timeout_ns.max(0) as u64));
    })?;

    // A port forwarding rule from QEMU's command line (hostfwd=...).
    // Returns 0, or -1 after printing why the rule cannot be used.
    linker.func_wrap(
        "env",
        "host_net_hostfwd",
        move |mut caller: Caller<'_, QemuHostState>, ptr: u32, len: u32| -> i32 {
            let Some(memory) = guest_memory(&mut caller) else {
                return -1;
            };
            let start = ptr as usize;
            let Some(bytes) = memory.data(&caller).get(start..start.saturating_add(len as usize)) else {
                return -1;
            };
            let rule = String::from_utf8_lossy(bytes).into_owned();
            match net.add_forward(&rule) {
                Ok((host, guest)) => {
                    eprintln!("[qemu-net] forwarding {host} to the guest's {guest}");
                    0
                }
                Err(e) => {
                    eprintln!("[qemu-net] hostfwd={rule}: {e}");
                    -1
                }
            }
        },
    )?;
    Ok(())
}

fn guest_memory(caller: &mut Caller<'_, QemuHostState>) -> Option<wasmtime::Memory> {
    caller.data().guest_memory.or_else(|| match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(m)) => Some(m),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
//  The two queues between QEMU's thread and the network thread
// ---------------------------------------------------------------------------

struct Shared {
    /// Frames from the guest, for the network thread.
    from_guest: Mutex<Vec<Vec<u8>>>,
    /// Frames for the guest, waiting for QEMU to fetch them.
    to_guest: Mutex<VecDeque<Vec<u8>>>,
    /// Length of `to_guest`, so that QEMU's frequent polls need no lock.
    to_guest_len: AtomicUsize,
    /// Signalled when a frame is added to `to_guest`, for `host_net_wait`.
    to_guest_ready: Condvar,
    /// Set by the network thread when `to_guest` was too full to take all
    /// it had: QEMU then wakes it once the guest has taken enough frames.
    wake_when_drained: AtomicBool,
    /// Port forwards set up by QEMU, for the network thread to take on.
    new_forwards: Mutex<Vec<(mio::net::TcpListener, SocketAddrV4)>>,
    waker: Waker,
    stop: AtomicBool,
}

/// The host end of QEMU's user network; see the module documentation.
pub struct VirtualNet {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VirtualNet {
    pub fn start() -> io::Result<Self> {
        let poll = Poll::new()?;
        let shared = Arc::new(Shared {
            from_guest: Mutex::new(Vec::new()),
            to_guest: Mutex::new(VecDeque::new()),
            to_guest_len: AtomicUsize::new(0),
            to_guest_ready: Condvar::new(),
            wake_when_drained: AtomicBool::new(false),
            new_forwards: Mutex::new(Vec::new()),
            waker: Waker::new(poll.registry(), WAKE)?,
            stop: AtomicBool::new(false),
        });
        let stack = Stack::new(shared.clone(), poll.registry().try_clone()?);
        let thread = std::thread::Builder::new()
            .name("qemu-net".into())
            .spawn(move || stack.run(poll))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Take a frame the guest sent.
    fn from_guest(&self, frame: &[u8]) {
        let mut queue = self.shared.from_guest.lock().unwrap();
        if queue.len() >= QUEUE_MAX {
            return;
        }
        queue.push(frame.to_vec());
        // The network thread empties the queue each time it wakes, so it
        // only needs waking for the first frame.
        if queue.len() == 1 {
            let _ = self.shared.waker.wake();
        }
    }

    /// The next frame for the guest, if there is one.
    fn to_guest(&self) -> Option<Vec<u8>> {
        if self.shared.to_guest_len.load(Ordering::Acquire) == 0 {
            return None;
        }
        let mut queue = self.shared.to_guest.lock().unwrap();
        let frame = queue.pop_front();
        self.shared.to_guest_len.store(queue.len(), Ordering::Release);
        if queue.len() < QUEUE_MAX / 2 && self.shared.wake_when_drained.swap(false, Ordering::AcqRel) {
            let _ = self.shared.waker.wake();
        }
        frame
    }

    /// Set up a port forward for a QEMU hostfwd rule. Returns the host
    /// address listened on, and the guest address forwarded to.
    fn add_forward(&self, rule: &str) -> Result<(SocketAddr, SocketAddrV4), String> {
        let (host, guest) = tcp::parse_hostfwd(rule)?;
        let listener =
            std::net::TcpListener::bind(host).map_err(|e| format!("cannot listen on {host}: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let bound = listener.local_addr().map_err(|e| e.to_string())?;
        let listener = mio::net::TcpListener::from_std(listener);
        self.shared.new_forwards.lock().unwrap().push((listener, guest));
        let _ = self.shared.waker.wake();
        Ok((bound, guest))
    }

    /// Sleep for up to `timeout`, or until there is a frame for the guest.
    fn wait_for_frame(&self, timeout: Duration) {
        let queue = self.shared.to_guest.lock().unwrap();
        let _ = self
            .shared
            .to_guest_ready
            .wait_timeout_while(queue, timeout, |queue| queue.is_empty());
    }
}

impl Drop for VirtualNet {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.shared.waker.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---------------------------------------------------------------------------
//  The network, as the guest sees it (runs on the network thread)
// ---------------------------------------------------------------------------

/// What a host socket carries.
#[derive(Clone, Copy)]
enum Flow {
    /// Pings from `guest` with ICMP identifier `ident`.
    Ping { guest: Ipv4Addr, ident: u16 },
    /// UDP between `guest` and `remote`, the address the guest sends to.
    Udp {
        guest: SocketAddrV4,
        remote: SocketAddrV4,
    },
}

struct HostSocket {
    /// A UDP socket, or for pings an ICMP socket (see `open_ping_socket`).
    socket: UdpSocket,
    flow: Flow,
    idle_timeout: Duration,
    last_used: Instant,
}

/// An IPv4 packet from the guest.
struct Packet<'a> {
    src: Ipv4Addr,
    dst: Ipv4Addr,
    /// The whole packet, header included.
    bytes: &'a [u8],
    header_len: usize,
}

impl Packet<'_> {
    fn payload(&self) -> &[u8] {
        &self.bytes[self.header_len..]
    }
}

/// Identifies the fragments of one IPv4 packet.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FragmentKey {
    src: Ipv4Addr,
    dst: Ipv4Addr,
    protocol: u8,
    id: u16,
}

/// A packet arriving in fragments.
struct Reassembly {
    /// The first fragment's IP header, once it has arrived.
    header: Vec<u8>,
    /// (offset, data) of each fragment so far.
    parts: Vec<(usize, Vec<u8>)>,
    /// Payload length, known from the last fragment.
    len: Option<usize>,
    started: Instant,
}

struct Stack {
    shared: Arc<Shared>,
    registry: Registry,
    sockets: HashMap<Token, HostSocket>,
    pings: HashMap<(Ipv4Addr, u16), Token>,
    udp_flows: HashMap<(SocketAddrV4, SocketAddrV4), Token>,
    fragments: HashMap<FragmentKey, Reassembly>,
    next_token: usize,
    last_sweep: Instant,
    guest_mac: Option<[u8; 6]>,
    ip_id: u16,
    /// The host's DNS server, from /etc/resolv.conf.
    nameserver: Option<SocketAddr>,
    tcp: tcp::TcpNat,
    /// QEMU_NET_DEBUG=1 logs every packet handled.
    debug: bool,
    warned_ping: bool,
}

impl Stack {
    fn new(shared: Arc<Shared>, registry: Registry) -> Self {
        let debug = std::env::var_os("QEMU_NET_DEBUG").is_some();
        Self {
            shared,
            registry,
            sockets: HashMap::new(),
            pings: HashMap::new(),
            udp_flows: HashMap::new(),
            fragments: HashMap::new(),
            next_token: 1,
            last_sweep: Instant::now(),
            guest_mac: None,
            ip_id: 0,
            nameserver: host_nameserver(),
            tcp: tcp::TcpNat::new(debug),
            debug,
            warned_ping: false,
        }
    }

    fn log(&self, args: fmt::Arguments) {
        if self.debug {
            eprintln!("[qemu-net] {args}");
        }
    }

    fn run(mut self, mut poll: Poll) {
        let mut events = Events::with_capacity(64);
        while !self.shared.stop.load(Ordering::Relaxed) {
            // Wake up for TCP's timers, and now and then to close idle sockets
            // and drop stale fragments, if there are any.
            let busy = !self.sockets.is_empty() || !self.fragments.is_empty();
            let mut timeout = busy.then_some(Duration::from_secs(5));
            if let Some(tcp) = self.tcp.poll_delay() {
                timeout = Some(timeout.map_or(tcp, |t| t.min(tcp)));
            }
            if self.tcp.starved() {
                // TCP has packets the guest's queue could not take, and wants
                // to run at once; QEMU wakes this thread once there is room
                // (see wake_when_drained). Meanwhile, retry now and then.
                timeout = Some(Duration::from_millis(20));
            }
            if let Err(e) = poll.poll(&mut events, timeout) {
                if e.kind() == ErrorKind::Interrupted {
                    continue;
                }
                eprintln!("[qemu-net] network thread stopped: {e}");
                return;
            }
            for event in events.iter() {
                match event.token() {
                    WAKE => {}
                    token if self.sockets.contains_key(&token) => self.socket_readable(token),
                    _ => {
                        if let Some(failed) = self.tcp.host_event(event, &self.registry, &mut self.next_token) {
                            self.tcp_failed(&failed);
                        }
                    }
                }
            }
            let forwards = std::mem::take(&mut *self.shared.new_forwards.lock().unwrap());
            for (listener, guest) in forwards {
                self.tcp.add_forward(listener, guest, &self.registry, &mut self.next_token);
            }
            let frames = std::mem::take(&mut *self.shared.from_guest.lock().unwrap());
            for frame in &frames {
                self.guest_frame(frame);
            }
            self.service_tcp();
            if self.last_sweep.elapsed() >= Duration::from_secs(1) {
                self.last_sweep = Instant::now();
                self.close_idle_sockets();
                self.fragments.retain(|_, r| r.started.elapsed() < REASSEMBLY_TIMEOUT);
            }
        }
    }

    // ----- frames from the guest -------------------------------------------

    fn guest_frame(&mut self, frame: &[u8]) {
        if frame.len() < 14 {
            return;
        }
        // There is one guest NIC; learn its address from what it sends.
        if frame[6] & 1 == 0 {
            let mac: [u8; 6] = frame[6..12].try_into().unwrap();
            if self.guest_mac.is_none() {
                eprintln!(
                    "[qemu-net] guest NIC {} is up; DNS queries go to {}",
                    mac_string(&mac),
                    self.nameserver
                        .map_or("nowhere (no nameserver in /etc/resolv.conf)".into(), |a| a.to_string())
                );
            }
            self.guest_mac = Some(mac);
        }
        let payload = &frame[14..];
        match u16::from_be_bytes([frame[12], frame[13]]) {
            ETH_ARP => self.arp(payload),
            ETH_IPV4 => self.ipv4(payload),
            _ => {} // IPv6 and the rest are not supported
        }
    }

    fn arp(&mut self, p: &[u8]) {
        // An Ethernet/IPv4 request: hardware type 1, protocol 0x0800,
        // address lengths 6 and 4, operation 1.
        if p.len() < 28 || p[..8] != [0, 1, 8, 0, 6, 4, 0, 1] {
            return;
        }
        let (sender_mac, sender_ip) = (&p[8..14], &p[14..18]);
        let target = ipv4(&p[24..28]);
        if target != GATEWAY_IP && target != DNS_IP {
            return;
        }
        self.log(format_args!("ARP: {target} is at {}", mac_string(&GATEWAY_MAC)));
        let mut reply = Vec::with_capacity(42);
        reply.extend_from_slice(sender_mac);
        reply.extend_from_slice(&GATEWAY_MAC);
        reply.extend_from_slice(&ETH_ARP.to_be_bytes());
        reply.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
        reply.extend_from_slice(&GATEWAY_MAC);
        reply.extend_from_slice(&target.octets());
        reply.extend_from_slice(sender_mac);
        reply.extend_from_slice(sender_ip);
        self.send_frame(reply);
    }

    fn ipv4(&mut self, p: &[u8]) {
        if p.len() < 20 || p[0] >> 4 != 4 {
            return;
        }
        let header_len = (p[0] & 0x0f) as usize * 4;
        let total_len = u16::from_be_bytes([p[2], p[3]]) as usize;
        if header_len < 20 || total_len < header_len || total_len > p.len() {
            return;
        }
        let fragment = u16::from_be_bytes([p[6], p[7]]);
        // A fragment: the more-fragments flag or an offset is set.
        if fragment & 0x3fff != 0 {
            if let Some((whole, header_len)) = self.reassemble(&p[..total_len], header_len, fragment) {
                self.ipv4_packet(&whole, header_len);
            }
            return;
        }
        self.ipv4_packet(&p[..total_len], header_len);
    }

    fn ipv4_packet(&mut self, bytes: &[u8], header_len: usize) {
        let packet = Packet {
            src: ipv4(&bytes[12..16]),
            dst: ipv4(&bytes[16..20]),
            bytes,
            header_len,
        };
        match bytes[9] {
            IP_ICMP => self.icmp(&packet),
            IP_UDP => self.udp(&packet),
            IP_TCP => self.tcp(&packet),
            _ => {}
        }
    }

    /// Collect a fragment; once a packet is complete, return it (with its
    /// header length).
    fn reassemble(&mut self, bytes: &[u8], header_len: usize, fragment: u16) -> Option<(Vec<u8>, usize)> {
        let key = FragmentKey {
            src: ipv4(&bytes[12..16]),
            dst: ipv4(&bytes[16..20]),
            protocol: bytes[9],
            id: u16::from_be_bytes([bytes[4], bytes[5]]),
        };
        let offset = (fragment & 0x1fff) as usize * 8;
        let data = &bytes[header_len..];
        if offset + data.len() + header_len > 65535 {
            self.fragments.remove(&key);
            return None;
        }
        if !self.fragments.contains_key(&key) && self.fragments.len() >= MAX_REASSEMBLIES {
            self.log(format_args!("dropped an IP fragment: too many packets in pieces"));
            return None;
        }
        let r = self.fragments.entry(key).or_insert_with(|| Reassembly {
            header: Vec::new(),
            parts: Vec::new(),
            len: None,
            started: Instant::now(),
        });
        if offset == 0 {
            r.header = bytes[..header_len].to_vec();
        }
        if fragment & 0x2000 == 0 {
            r.len = Some(offset + data.len());
        }
        r.parts.push((offset, data.to_vec()));

        // Complete once the first and last fragments are in, with no gap.
        let len = r.len?;
        if r.header.is_empty() {
            return None;
        }
        r.parts.sort_by_key(|(offset, _)| *offset);
        let mut covered = 0;
        for (offset, data) in &r.parts {
            if *offset > covered {
                return None;
            }
            covered = covered.max(offset + data.len());
        }
        if covered < len {
            return None;
        }

        let r = self.fragments.remove(&key)?;
        let header_len = r.header.len();
        let mut whole = r.header;
        whole.resize(header_len + len, 0);
        for (offset, data) in &r.parts {
            let end = (offset + data.len()).min(len);
            if *offset < end {
                whole[header_len + offset..header_len + end].copy_from_slice(&data[..end - offset]);
            }
        }
        whole[2..4].copy_from_slice(&((header_len + len) as u16).to_be_bytes());
        whole[6..8].fill(0);
        Some((whole, header_len))
    }

    fn icmp(&mut self, packet: &Packet) {
        let m = packet.payload();
        // Echo requests only.
        if m.len() < 8 || m[0] != 8 || m[1] != 0 {
            return;
        }
        let ident = u16::from_be_bytes([m[4], m[5]]);
        let seq = u16::from_be_bytes([m[6], m[7]]);
        self.log(format_args!("ping {} (id {ident}, seq {seq})", packet.dst));
        if packet.dst == GATEWAY_IP || packet.dst == DNS_IP {
            self.send_echo_reply(packet.dst, packet.src, ident, seq, &m[8..]);
        } else if is_routable(packet.dst) {
            self.ping_from_host(packet.src, packet.dst, ident, seq, &m[8..]);
        }
    }

    fn udp(&mut self, packet: &Packet) {
        let p = packet.payload();
        if p.len() < 8 {
            return;
        }
        let len = u16::from_be_bytes([p[4], p[5]]) as usize;
        if len < 8 || len > p.len() {
            return;
        }
        let guest = SocketAddrV4::new(packet.src, u16::from_be_bytes([p[0], p[1]]));
        let remote = SocketAddrV4::new(packet.dst, u16::from_be_bytes([p[2], p[3]]));
        let data = &p[8..len];
        if remote.port() == 67 {
            return self.dhcp(data);
        }
        match self.host_address(remote) {
            Some(host) => self.udp_from_guest(guest, remote, host, data),
            // Nothing listens there: say so, unless it was a broadcast.
            None if is_unicast(packet.dst) => {
                self.log(format_args!("UDP to {remote}: port unreachable"));
                self.send_unreachable(packet, 3);
            }
            None => {}
        }
    }

    fn tcp(&mut self, packet: &Packet) {
        let p = packet.payload();
        if p.len() < 20 || !is_unicast(packet.dst) {
            return;
        }
        let flow = tcp::Flow {
            guest: SocketAddrV4::new(packet.src, u16::from_be_bytes([p[0], p[1]])),
            remote: SocketAddrV4::new(packet.dst, u16::from_be_bytes([p[2], p[3]])),
        };
        let host = self.host_address(flow.remote);
        match self.tcp.from_guest(packet.bytes, flow, p[13], host, &self.registry, &mut self.next_token) {
            tcp::Verdict::Done => {}
            tcp::Verdict::Reset => self.send_tcp_reset(packet),
            tcp::Verdict::Unreachable => self.send_unreachable(packet, 1),
        }
    }

    /// The host side of a guest's connection attempt failed: tell the guest,
    /// as a host on a real network would.
    fn tcp_failed(&mut self, failed: &tcp::Failed) {
        let syn = &failed.syn;
        if syn.len() < 20 {
            return;
        }
        let packet = Packet {
            src: ipv4(&syn[12..16]),
            dst: ipv4(&syn[16..20]),
            bytes: syn,
            header_len: (syn[0] & 0x0f) as usize * 4,
        };
        if failed.refused {
            self.send_tcp_reset(&packet);
        } else {
            self.send_unreachable(&packet, 1);
        }
    }

    /// Run TCP, and pass the packets it produced to the guest.
    fn service_tcp(&mut self) {
        let queued = self.shared.to_guest_len.load(Ordering::Acquire);
        self.tcp.service(QUEUE_MAX.saturating_sub(queued), &self.registry);
        for packet in self.tcp.take_packets() {
            self.send_ip_packet(packet);
        }
        if self.tcp.starved() {
            self.shared.wake_when_drained.store(true, Ordering::Release);
        }
    }

    /// Answer a TCP segment with a reset, as a host with nothing listening
    /// on the port would.
    fn send_tcp_reset(&mut self, packet: &Packet) {
        const FIN: u8 = 0x01;
        const SYN: u8 = 0x02;
        const RST: u8 = 0x04;
        const ACK: u8 = 0x10;

        let p = packet.payload();
        if p.len() < 20 {
            return;
        }
        let flags = p[13];
        let data_offset = (p[12] >> 4) as usize * 4;
        if flags & RST != 0 || data_offset < 20 || data_offset > p.len() {
            return;
        }
        if flags & SYN != 0 {
            let port = u16::from_be_bytes([p[2], p[3]]);
            self.log(format_args!("TCP connection to {}:{port} refused", packet.dst));
        }
        // RFC 793: a reset acknowledges what the segment occupied, or
        // takes its sequence number from the segment's acknowledgement.
        let seq = u32::from_be_bytes(p[4..8].try_into().unwrap());
        let ack = u32::from_be_bytes(p[8..12].try_into().unwrap());
        let seg_len = (p.len() - data_offset) as u32
            + u32::from(flags & SYN != 0)
            + u32::from(flags & FIN != 0);
        let (rst_seq, rst_ack, rst_flags) = if flags & ACK != 0 {
            (ack, 0, RST)
        } else {
            (0, seq.wrapping_add(seg_len), RST | ACK)
        };
        let mut seg = [0u8; 20];
        seg[0..2].copy_from_slice(&p[2..4]);
        seg[2..4].copy_from_slice(&p[0..2]);
        seg[4..8].copy_from_slice(&rst_seq.to_be_bytes());
        seg[8..12].copy_from_slice(&rst_ack.to_be_bytes());
        seg[12] = 5 << 4;
        seg[13] = rst_flags;
        let sum = transport_checksum(packet.dst, packet.src, IP_TCP, &seg);
        seg[16..18].copy_from_slice(&sum.to_be_bytes());
        self.send_ipv4(packet.dst, packet.src, IP_TCP, &seg);
    }

    /// Answer a DHCP request: the guest is always offered 10.0.2.15.
    fn dhcp(&mut self, m: &[u8]) {
        const MAGIC: [u8; 4] = [99, 130, 83, 99];
        const DISCOVER: u8 = 1;
        const OFFER: u8 = 2;
        const REQUEST: u8 = 3;
        const ACK: u8 = 5;
        const NAK: u8 = 6;

        // A BOOTP request from an Ethernet client, with DHCP options.
        if m.len() < 240 || m[..3] != [1, 1, 6] || m[236..240] != MAGIC {
            return;
        }
        let (mut msg_type, mut requested, mut server) = (0, None, None);
        for (code, value) in dhcp_options(&m[240..]) {
            match (code, value.len()) {
                (53, 1) => msg_type = value[0],
                (50, 4) => requested = Some(ipv4(value)),
                (54, 4) => server = Some(ipv4(value)),
                _ => {}
            }
        }
        let client_ip = ipv4(&m[12..16]);
        let reply_type = match msg_type {
            DISCOVER => OFFER,
            // Ignore a client taking another server's offer (there is none).
            REQUEST if server.is_some_and(|s| s != GATEWAY_IP) => return,
            REQUEST if requested.unwrap_or(client_ip) == GUEST_IP => ACK,
            REQUEST => NAK,
            _ => return, // DECLINE, RELEASE, INFORM
        };

        let mut r = vec![0u8; 240];
        r[..3].copy_from_slice(&[2, 1, 6]); // BOOTREPLY for an Ethernet client
        r[4..8].copy_from_slice(&m[4..8]); // transaction ID
        r[10..12].copy_from_slice(&m[10..12]); // flags
        r[12..16].copy_from_slice(&m[12..16]); // client's current address
        if reply_type != NAK {
            r[16..20].copy_from_slice(&GUEST_IP.octets());
            r[20..24].copy_from_slice(&GATEWAY_IP.octets());
        }
        r[28..44].copy_from_slice(&m[28..44]); // client hardware address
        r[236..240].copy_from_slice(&MAGIC);
        push_dhcp_option(&mut r, 53, &[reply_type]);
        push_dhcp_option(&mut r, 54, &GATEWAY_IP.octets());
        if reply_type != NAK {
            push_dhcp_option(&mut r, 51, &DHCP_LEASE_SECS.to_be_bytes());
            push_dhcp_option(&mut r, 1, &NETMASK.octets());
            push_dhcp_option(&mut r, 3, &GATEWAY_IP.octets());
            push_dhcp_option(&mut r, 6, &DNS_IP.octets());
            push_dhcp_option(&mut r, 28, &BROADCAST_IP.octets());
        }
        r.push(255);
        // Some clients insist on at least the 300 bytes of a BOOTP message.
        r.resize(r.len().max(300), 0);

        let client_mac: [u8; 6] = m[28..34].try_into().unwrap();
        match reply_type {
            ACK => eprintln!("[qemu-net] DHCP: leased {GUEST_IP} to {}", mac_string(&client_mac)),
            NAK => eprintln!("[qemu-net] DHCP: refused {:?} to {}", requested, mac_string(&client_mac)),
            _ => self.log(format_args!("DHCP: offered {GUEST_IP}")),
        }
        // Until it has an address the client can only take broadcasts.
        let dst = if client_ip.is_unspecified() { Ipv4Addr::BROADCAST } else { client_ip };
        self.send_udp_to(
            client_mac,
            SocketAddrV4::new(GATEWAY_IP, 67),
            SocketAddrV4::new(dst, 68),
            &r,
        );
    }

    /// Where traffic the guest sends to `addr` goes on the host, if anywhere.
    fn host_address(&self, addr: SocketAddrV4) -> Option<SocketAddr> {
        let ip = *addr.ip();
        if ip == GATEWAY_IP {
            Some(SocketAddr::from((Ipv4Addr::LOCALHOST, addr.port())))
        } else if ip == DNS_IP {
            self.nameserver.filter(|_| addr.port() == 53)
        } else if is_routable(ip) {
            Some(addr.into())
        } else {
            None
        }
    }

    // ----- host sockets ----------------------------------------------------

    fn add_socket(&mut self, mut socket: UdpSocket, flow: Flow, idle_timeout: Duration) -> Option<Token> {
        let token = Token(self.next_token);
        self.next_token += 1;
        if let Err(e) = self.registry.register(&mut socket, token, Interest::READABLE) {
            eprintln!("[qemu-net] cannot watch a host socket: {e}");
            return None;
        }
        self.sockets.insert(
            token,
            HostSocket {
                socket,
                flow,
                idle_timeout,
                last_used: Instant::now(),
            },
        );
        Some(token)
    }

    fn close_idle_sockets(&mut self) {
        let now = Instant::now();
        let idle: Vec<Token> = self
            .sockets
            .iter()
            .filter(|(_, s)| now - s.last_used > s.idle_timeout)
            .map(|(token, _)| *token)
            .collect();
        for token in idle {
            let Some(mut s) = self.sockets.remove(&token) else {
                continue;
            };
            let _ = self.registry.deregister(&mut s.socket);
            match s.flow {
                Flow::Ping { guest, ident } => self.pings.remove(&(guest, ident)),
                Flow::Udp { guest, remote } => self.udp_flows.remove(&(guest, remote)),
            };
        }
    }

    /// Ping `dst` from the host on the guest's behalf.
    fn ping_from_host(&mut self, guest: Ipv4Addr, dst: Ipv4Addr, ident: u16, seq: u16, data: &[u8]) {
        let token = match self.pings.get(&(guest, ident)) {
            Some(&token) => token,
            None => {
                let socket = match open_ping_socket() {
                    Ok(socket) => socket,
                    Err(e) => {
                        if !self.warned_ping {
                            self.warned_ping = true;
                            eprintln!(
                                "[qemu-net] cannot ping from the host: {e} (on Linux, the \
                                 user's group must be in net.ipv4.ping_group_range)"
                            );
                        }
                        return;
                    }
                };
                let Some(token) = self.add_socket(socket, Flow::Ping { guest, ident }, IDLE_TIMEOUT) else {
                    return;
                };
                self.pings.insert((guest, ident), token);
                token
            }
        };
        let s = self.sockets.get_mut(&token).unwrap();
        s.last_used = Instant::now();
        // The kernel fills in the identifier and the checksum.
        let mut msg = Vec::with_capacity(8 + data.len());
        msg.extend_from_slice(&[8, 0, 0, 0, 0, 0]);
        msg.extend_from_slice(&seq.to_be_bytes());
        msg.extend_from_slice(data);
        if let Err(e) = s.socket.send_to(&msg, SocketAddr::from((dst, 0))) {
            self.log(format_args!("ping {dst} from the host: {e}"));
        }
    }

    /// Send a datagram from the guest out through a host socket.
    fn udp_from_guest(&mut self, guest: SocketAddrV4, remote: SocketAddrV4, host: SocketAddr, data: &[u8]) {
        let token = match self.udp_flows.get(&(guest, remote)) {
            Some(&token) => token,
            None => {
                let socket = match open_udp_socket(host) {
                    Ok(socket) => socket,
                    Err(e) => {
                        self.log(format_args!("UDP to {host}: {e}"));
                        return;
                    }
                };
                let timeout = if *remote.ip() == DNS_IP { DNS_IDLE_TIMEOUT } else { IDLE_TIMEOUT };
                let Some(token) = self.add_socket(socket, Flow::Udp { guest, remote }, timeout) else {
                    return;
                };
                self.udp_flows.insert((guest, remote), token);
                token
            }
        };
        self.log(format_args!("UDP {guest} -> {remote} ({host}): {} bytes", data.len()));
        let s = self.sockets.get_mut(&token).unwrap();
        s.last_used = Instant::now();
        if let Err(e) = s.socket.send(data) {
            self.log(format_args!("UDP to {host}: {e}"));
        }
    }

    /// Relay whatever arrived on a host socket to the guest.
    fn socket_readable(&mut self, token: Token) {
        let Some(s) = self.sockets.get_mut(&token) else {
            return;
        };
        let flow = s.flow;
        let mut received = Vec::new();
        let mut buf = vec![0u8; 65536];
        let mut errors = 0;
        // Readiness is edge-triggered: read until the socket is drained.
        loop {
            match s.socket.recv_from(&mut buf) {
                Ok((n, from)) => received.push((buf[..n].to_vec(), from)),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // An ICMP error for an earlier datagram is reported once.
                Err(_) if errors < 8 => errors += 1,
                Err(_) => break,
            }
        }
        if !received.is_empty() {
            s.last_used = Instant::now();
        }
        for (data, from) in received {
            match flow {
                Flow::Ping { guest, ident } => {
                    // Ping sockets return the ICMP message without its IP header.
                    let IpAddr::V4(from) = from.ip() else { continue };
                    if data.len() < 8 || data[0] != 0 {
                        continue; // not an echo reply
                    }
                    let seq = u16::from_be_bytes([data[6], data[7]]);
                    self.log(format_args!("ping reply from {from} (seq {seq})"));
                    self.send_echo_reply(from, guest, ident, seq, &data[8..]);
                }
                Flow::Udp { guest, remote } => {
                    self.log(format_args!("UDP {remote} -> {guest}: {} bytes", data.len()));
                    self.send_udp(remote, guest, &data);
                }
            }
        }
    }

    // ----- frames to the guest ---------------------------------------------

    fn send_frame(&self, mut frame: Vec<u8>) {
        // Pad to the minimum Ethernet frame size (less the checksum).
        frame.resize(frame.len().max(60), 0);
        let mut queue = self.shared.to_guest.lock().unwrap();
        if queue.len() < QUEUE_MAX {
            queue.push_back(frame);
            self.shared.to_guest_len.store(queue.len(), Ordering::Release);
            self.shared.to_guest_ready.notify_one();
        }
    }

    /// Send a complete IPv4 packet (from the TCP stack) to the guest.
    fn send_ip_packet(&self, packet: Vec<u8>) {
        let Some(mac) = self.guest_mac else {
            return;
        };
        let mut frame = Vec::with_capacity(14 + packet.len());
        frame.extend_from_slice(&mac);
        frame.extend_from_slice(&GATEWAY_MAC);
        frame.extend_from_slice(&ETH_IPV4.to_be_bytes());
        frame.extend_from_slice(&packet);
        self.send_frame(frame);
    }

    fn send_ipv4(&mut self, src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, payload: &[u8]) {
        let Some(mac) = self.guest_mac else {
            return;
        };
        self.send_ipv4_to(mac, src, dst, protocol, payload);
    }

    /// Send an IP packet to the guest, in fragments if it exceeds the MTU.
    fn send_ipv4_to(&mut self, mac: [u8; 6], src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, payload: &[u8]) {
        if payload.len() > 65535 - 20 {
            return;
        }
        self.ip_id = self.ip_id.wrapping_add(1);
        // Fragment offsets count 8-byte units.
        let max_len = (MTU - 20) & !7;
        let mut offset = 0;
        loop {
            let len = (payload.len() - offset).min(max_len);
            let more = offset + len < payload.len();
            let mut header = [0u8; 20];
            header[0] = 0x45;
            header[2..4].copy_from_slice(&((20 + len) as u16).to_be_bytes());
            header[4..6].copy_from_slice(&self.ip_id.to_be_bytes());
            let flags_offset = (offset / 8) as u16 | if more { 0x2000 } else { 0 };
            header[6..8].copy_from_slice(&flags_offset.to_be_bytes());
            header[8] = 64; // TTL
            header[9] = protocol;
            header[12..16].copy_from_slice(&src.octets());
            header[16..20].copy_from_slice(&dst.octets());
            let sum = checksum(&[&header]);
            header[10..12].copy_from_slice(&sum.to_be_bytes());

            let mut frame = Vec::with_capacity(14 + 20 + len);
            frame.extend_from_slice(&mac);
            frame.extend_from_slice(&GATEWAY_MAC);
            frame.extend_from_slice(&ETH_IPV4.to_be_bytes());
            frame.extend_from_slice(&header);
            frame.extend_from_slice(&payload[offset..offset + len]);
            self.send_frame(frame);

            offset += len;
            if !more {
                break;
            }
        }
    }

    fn send_udp(&mut self, src: SocketAddrV4, dst: SocketAddrV4, data: &[u8]) {
        let Some(mac) = self.guest_mac else {
            return;
        };
        self.send_udp_to(mac, src, dst, data);
    }

    fn send_udp_to(&mut self, mac: [u8; 6], src: SocketAddrV4, dst: SocketAddrV4, data: &[u8]) {
        let len = 8 + data.len();
        if len > 65535 - 20 {
            return;
        }
        let mut d = Vec::with_capacity(len);
        d.extend_from_slice(&src.port().to_be_bytes());
        d.extend_from_slice(&dst.port().to_be_bytes());
        d.extend_from_slice(&(len as u16).to_be_bytes());
        d.extend_from_slice(&[0, 0]);
        d.extend_from_slice(data);
        // A computed checksum of 0 is sent as 0xffff (0 means "none").
        let sum = match transport_checksum(*src.ip(), *dst.ip(), IP_UDP, &d) {
            0 => 0xffff,
            sum => sum,
        };
        d[6..8].copy_from_slice(&sum.to_be_bytes());
        self.send_ipv4_to(mac, *src.ip(), *dst.ip(), IP_UDP, &d);
    }

    fn send_echo_reply(&mut self, src: Ipv4Addr, dst: Ipv4Addr, ident: u16, seq: u16, data: &[u8]) {
        let mut m = Vec::with_capacity(8 + data.len());
        m.extend_from_slice(&[0, 0, 0, 0]);
        m.extend_from_slice(&ident.to_be_bytes());
        m.extend_from_slice(&seq.to_be_bytes());
        m.extend_from_slice(data);
        let sum = checksum(&[&m]);
        m[2..4].copy_from_slice(&sum.to_be_bytes());
        self.send_ipv4(src, dst, IP_ICMP, &m);
    }

    /// Send an ICMP "destination unreachable" for `packet`, quoting its IP
    /// header and the start of its payload as the guest expects.
    fn send_unreachable(&mut self, packet: &Packet, code: u8) {
        let quoted = &packet.bytes[..packet.bytes.len().min(packet.header_len + 8)];
        let mut m = Vec::with_capacity(8 + quoted.len());
        m.extend_from_slice(&[3, code, 0, 0, 0, 0, 0, 0]);
        m.extend_from_slice(quoted);
        let sum = checksum(&[&m]);
        m[2..4].copy_from_slice(&sum.to_be_bytes());
        self.send_ipv4(packet.dst, packet.src, IP_ICMP, &m);
    }
}

// ---------------------------------------------------------------------------
//  Helpers
// ---------------------------------------------------------------------------

/// An unprivileged ICMP ("ping") socket. Mio has no type for these, but its
/// UdpSocket is a thin wrapper around a datagram socket's file descriptor,
/// which is all that is needed.
fn open_ping_socket() -> io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::ICMPV4))?;
    socket.set_nonblocking(true)?;
    Ok(UdpSocket::from_std(socket.into()))
}

/// A UDP socket that sends to and receives from `host` only.
fn open_udp_socket(host: SocketAddr) -> io::Result<UdpSocket> {
    let any: SocketAddr = match host {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    let socket = UdpSocket::bind(any)?;
    socket.connect(host)?;
    Ok(socket)
}

/// The first nameserver in /etc/resolv.conf.
fn host_nameserver() -> Option<SocketAddr> {
    let conf = std::fs::read_to_string("/etc/resolv.conf").ok()?;
    conf.lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver"))
        .find_map(|addr| addr.trim().parse::<IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 53))
}

/// Whether traffic to `ip` leaves the virtual network.
fn is_routable(ip: Ipv4Addr) -> bool {
    is_unicast(ip) && !in_virtual_network(ip)
}

fn is_unicast(ip: Ipv4Addr) -> bool {
    !(ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || ip == BROADCAST_IP)
}

fn in_virtual_network(ip: Ipv4Addr) -> bool {
    ip.octets()[..3] == GATEWAY_IP.octets()[..3]
}

fn ipv4(bytes: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])
}

fn mac_string(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// DHCP options as (code, value) pairs, up to the end option.
fn dhcp_options(options: &[u8]) -> Vec<(u8, &[u8])> {
    let mut found = Vec::new();
    let mut i = 0;
    while let Some(&code) = options.get(i) {
        match code {
            0 => {
                i += 1; // padding
                continue;
            }
            255 => break,
            _ => {}
        }
        let Some(&len) = options.get(i + 1) else { break };
        let Some(value) = options.get(i + 2..i + 2 + len as usize) else { break };
        found.push((code, value));
        i += 2 + len as usize;
    }
    found
}

fn push_dhcp_option(message: &mut Vec<u8>, code: u8, value: &[u8]) {
    message.push(code);
    message.push(value.len() as u8);
    message.extend_from_slice(value);
}

/// The Internet checksum of the concatenated `parts`.
fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    let mut pending: Option<u8> = None;
    for &byte in parts.iter().flat_map(|part| part.iter()) {
        match pending.take() {
            Some(high) => sum += u32::from(u16::from_be_bytes([high, byte])),
            None => pending = Some(byte),
        }
    }
    if let Some(high) = pending {
        sum += u32::from(high) << 8;
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Checksum of a TCP or UDP segment, including the IPv4 pseudo-header.
fn transport_checksum(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, segment: &[u8]) -> u16 {
    let len = (segment.len() as u16).to_be_bytes();
    checksum(&[&src.octets(), &dst.octets(), &[0, protocol], &len, segment])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_stack() -> (Stack, Poll) {
        let poll = Poll::new().unwrap();
        let shared = Arc::new(Shared {
            from_guest: Mutex::new(Vec::new()),
            to_guest: Mutex::new(VecDeque::new()),
            to_guest_len: AtomicUsize::new(0),
            to_guest_ready: Condvar::new(),
            wake_when_drained: AtomicBool::new(false),
            new_forwards: Mutex::new(Vec::new()),
            waker: Waker::new(poll.registry(), WAKE).unwrap(),
            stop: AtomicBool::new(false),
        });
        let stack = Stack::new(shared, poll.registry().try_clone().unwrap());
        (stack, poll)
    }

    /// Fragment `payload` as the guest would: `parts` are (offset, len).
    fn fragment(payload: &[u8], parts: &[(usize, usize)]) -> Vec<Vec<u8>> {
        parts
            .iter()
            .map(|&(offset, len)| {
                let more = offset + len < payload.len();
                let mut p = vec![0u8; 20];
                p[0] = 0x45;
                p[2..4].copy_from_slice(&((20 + len) as u16).to_be_bytes());
                p[4..6].copy_from_slice(&0x1234u16.to_be_bytes());
                let flags = (offset / 8) as u16 | if more { 0x2000 } else { 0 };
                p[6..8].copy_from_slice(&flags.to_be_bytes());
                p[8] = 64;
                p[9] = IP_ICMP;
                p[12..16].copy_from_slice(&GUEST_IP.octets());
                p[16..20].copy_from_slice(&[1, 1, 1, 1]);
                p.extend_from_slice(&payload[offset..offset + len]);
                p
            })
            .collect()
    }

    #[test]
    fn reassembles_fragments_in_any_order() {
        let (mut stack, _poll) = test_stack();
        let payload: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let parts = fragment(&payload, &[(1480, 1480), (2960, 40), (0, 1480)]);
        let mut result = None;
        for (i, p) in parts.iter().enumerate() {
            result = stack.reassemble(p, 20, u16::from_be_bytes([p[6], p[7]]));
            assert_eq!(result.is_some(), i == parts.len() - 1);
        }
        let (whole, header_len) = result.unwrap();
        assert_eq!(header_len, 20);
        assert_eq!(&whole[20..], &payload[..]);
        assert_eq!(u16::from_be_bytes([whole[2], whole[3]]), 3020);
        assert_eq!(&whole[6..8], &[0, 0]);
        assert!(stack.fragments.is_empty());
    }

    #[test]
    fn fragments_to_the_guest_reassemble() {
        let (mut stack, _poll) = test_stack();
        let payload: Vec<u8> = (0..4000u32).map(|i| (i * 7) as u8).collect();
        stack.send_ipv4_to([2, 0, 0, 0, 0, 1], GATEWAY_IP, GUEST_IP, IP_UDP, &payload);
        let frames: Vec<Vec<u8>> = stack.shared.to_guest.lock().unwrap().drain(..).collect();
        assert_eq!(frames.len(), 3);
        let mut result = None;
        for frame in frames.iter().rev() {
            let ip = &frame[14..];
            let len = u16::from_be_bytes([ip[2], ip[3]]) as usize;
            assert!(len <= MTU);
            assert_eq!(checksum(&[&ip[..20]]), 0, "header checksum");
            result = stack.reassemble(&ip[..len], 20, u16::from_be_bytes([ip[6], ip[7]]));
        }
        let (whole, header_len) = result.unwrap();
        assert_eq!(&whole[header_len..], &payload[..]);
    }

    #[test]
    fn checksum_matches_rfc1071_example() {
        // RFC 1071 section 3: the sum of these words is 0xddf2.
        let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(checksum(&[&data]), !0xddf2);
        // Splitting the data at an odd offset must not change the result.
        assert_eq!(checksum(&[&data[..3], &data[3..]]), !0xddf2);
    }

    #[test]
    fn dhcp_options_stop_at_end_or_truncation() {
        let opts = [53, 1, 3, 0, 50, 4, 10, 0, 2, 15, 255, 1, 1, 1];
        let found = dhcp_options(&opts);
        assert_eq!(found, vec![(53, &[3u8][..]), (50, &[10u8, 0, 2, 15][..])]);
        assert_eq!(dhcp_options(&[54, 4, 10, 0]), vec![]);
    }

    // ----- TCP, end to end: a guest made of smoltcp talks through a running
    // VirtualNet to real sockets on the host's loopback interface -----------

    use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
    use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
    use smoltcp::socket::tcp as stcp;
    use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};
    use std::io::{Read, Write};

    const TEST_GUEST_MAC: [u8; 6] = [0x52, 0x54, 0, 0x12, 0x34, 0x56];

    /// The guest's network card: Ethernet frames to and from a VirtualNet,
    /// IP packets for smoltcp.
    struct GuestNic<'a>(&'a VirtualNet);
    struct GuestRx(Vec<u8>);
    struct GuestTx<'a>(&'a VirtualNet);

    impl phy::RxToken for GuestRx {
        fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
            f(&self.0[14..])
        }
    }

    impl phy::TxToken for GuestTx<'_> {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            let mut frame = vec![0; 14 + len];
            frame[..6].copy_from_slice(&GATEWAY_MAC);
            frame[6..12].copy_from_slice(&TEST_GUEST_MAC);
            frame[12..14].copy_from_slice(&ETH_IPV4.to_be_bytes());
            let result = f(&mut frame[14..]);
            self.0.from_guest(&frame);
            result
        }
    }

    impl Device for GuestNic<'_> {
        type RxToken<'a> = GuestRx where Self: 'a;
        type TxToken<'a> = GuestTx<'a> where Self: 'a;

        fn receive(&mut self, _now: smoltcp::time::Instant) -> Option<(GuestRx, GuestTx<'_>)> {
            loop {
                let frame = self.0.to_guest()?;
                if frame.len() > 14 && frame[12..14] == ETH_IPV4.to_be_bytes() {
                    return Some((GuestRx(frame), GuestTx(self.0)));
                }
            }
        }

        fn transmit(&mut self, _now: smoltcp::time::Instant) -> Option<GuestTx<'_>> {
            Some(GuestTx(self.0))
        }

        fn capabilities(&self) -> DeviceCapabilities {
            let mut caps = DeviceCapabilities::default();
            caps.medium = Medium::Ip;
            caps.max_transmission_unit = MTU;
            caps
        }
    }

    struct TestGuest {
        net: VirtualNet,
        iface: Interface,
        sockets: SocketSet<'static>,
    }

    impl TestGuest {
        fn new() -> Self {
            let net = VirtualNet::start().unwrap();
            let mut config = Config::new(HardwareAddress::Ip);
            config.random_seed = 7;
            let mut iface = Interface::new(config, &mut GuestNic(&net), smoltcp::time::Instant::now());
            iface.update_ip_addrs(|addrs| addrs.push(IpCidr::new(IpAddress::Ipv4(GUEST_IP), 24)).unwrap());
            iface.routes_mut().add_default_ipv4_route(GATEWAY_IP).unwrap();
            Self {
                net,
                iface,
                sockets: SocketSet::new(Vec::new()),
            }
        }

        fn socket(&mut self, rx: usize, tx: usize) -> SocketHandle {
            self.sockets.add(stcp::Socket::new(
                stcp::SocketBuffer::new(vec![0; rx]),
                stcp::SocketBuffer::new(vec![0; tx]),
            ))
        }

        fn tcp(&mut self, handle: SocketHandle) -> &mut stcp::Socket<'static> {
            self.sockets.get_mut(handle)
        }

        fn connect(&mut self, handle: SocketHandle, remote: SocketAddrV4, local_port: u16) {
            let cx = self.iface.context();
            self.sockets.get_mut::<stcp::Socket>(handle).connect(cx, remote, local_port).unwrap();
        }

        /// Send a frame so that the network learns the guest's MAC address,
        /// as DHCP would: an ARP request for the gateway.
        fn announce(&self) {
            let mut frame = vec![0xff; 6];
            frame.extend_from_slice(&TEST_GUEST_MAC);
            frame.extend_from_slice(&ETH_ARP.to_be_bytes());
            frame.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
            frame.extend_from_slice(&TEST_GUEST_MAC);
            frame.extend_from_slice(&GUEST_IP.octets());
            frame.extend_from_slice(&[0; 6]);
            frame.extend_from_slice(&GATEWAY_IP.octets());
            self.net.from_guest(&frame);
        }

        /// Run the guest's stack until `done` returns true (then returns
        /// true), or until `timeout` passes.
        fn run(&mut self, timeout: Duration, mut done: impl FnMut(&mut Self) -> bool) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                self.iface
                    .poll(smoltcp::time::Instant::now(), &mut GuestNic(&self.net), &mut self.sockets);
                if done(self) {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            false
        }
    }

    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32).map(|i| (i.wrapping_mul(2654435761) >> 13 ^ seed) as u8).collect()
    }

    /// A megabyte to a host echo server and back, then a close from the
    /// guest that the host sees.
    #[test]
    fn tcp_relays_both_ways() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let echo = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            let mut buf = vec![0; 65536];
            let mut total = 0;
            loop {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    return total;
                }
                stream.write_all(&buf[..n]).unwrap();
                total += n;
            }
        });

        let mut g = TestGuest::new();
        let h = g.socket(64 * 1024, 64 * 1024);
        g.connect(h, SocketAddrV4::new(GATEWAY_IP, port), 40000);
        let data = pattern(1_000_000, 1);
        let (mut sent, mut received) = (0, Vec::new());
        let finished = g.run(Duration::from_secs(60), |g| {
            let s = g.tcp(h);
            if s.can_send() && sent < data.len() {
                sent += s.send_slice(&data[sent..]).unwrap();
            }
            if s.can_recv() {
                s.recv(|b| {
                    received.extend_from_slice(b);
                    (b.len(), ())
                })
                .unwrap();
            }
            if received.len() == data.len() {
                s.close();
            }
            matches!(s.state(), stcp::State::TimeWait | stcp::State::Closed)
        });
        assert!(finished, "timed out: sent {sent}, received {}", received.len());
        assert!(received == data, "echoed data differs");
        assert_eq!(echo.join().unwrap(), data.len());
    }

    /// A connection to a closed port is refused with a reset.
    #[test]
    fn tcp_refused_connection_is_reset() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut g = TestGuest::new();
        let h = g.socket(4096, 4096);
        g.connect(h, SocketAddrV4::new(GATEWAY_IP, port), 40001);
        let mut established = false;
        let closed = g.run(Duration::from_secs(10), |g| {
            let s = g.tcp(h);
            established |= s.state() == stcp::State::Established;
            s.state() == stcp::State::Closed
        });
        assert!(closed && !established);
    }

    /// The host sends and closes first; the guest, reading slowly through a
    /// small window, gets everything and the FIN, then answers and closes.
    #[test]
    fn tcp_half_close_from_the_host() {
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let data = pattern(300_000, 2);
        let to_send = data.clone();
        let host = std::thread::spawn(move || {
            let (mut stream, _) = server.accept().unwrap();
            stream.write_all(&to_send).unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut answer = Vec::new();
            stream.read_to_end(&mut answer).unwrap();
            answer
        });

        let mut g = TestGuest::new();
        let h = g.socket(2048, 4096);
        g.connect(h, SocketAddrV4::new(GATEWAY_IP, port), 40002);
        let mut received = Vec::new();
        let mut answered = false;
        let finished = g.run(Duration::from_secs(60), |g| {
            let s = g.tcp(h);
            if s.can_recv() {
                s.recv(|b| {
                    let n = b.len().min(512);
                    received.extend_from_slice(&b[..n]);
                    (n, ())
                })
                .unwrap();
            }
            if !answered && s.state() == stcp::State::CloseWait && !s.can_recv() {
                s.send_slice(b"bye").unwrap();
                s.close();
                answered = true;
            }
            answered && s.state() == stcp::State::Closed
        });
        assert!(finished, "timed out after receiving {} bytes", received.len());
        assert!(received == data, "received data differs");
        assert_eq!(host.join().unwrap(), b"bye");
    }

    /// Many connections to the same host port at once each get their own
    /// host connection.
    #[test]
    fn tcp_parallel_connections() {
        const N: usize = 20;
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        let host = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for _ in 0..N {
                let (mut stream, _) = server.accept().unwrap();
                handlers.push(std::thread::spawn(move || {
                    let mut message = Vec::new();
                    stream.read_to_end(&mut message).unwrap();
                    message.reverse();
                    stream.write_all(&message).unwrap();
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });

        let mut g = TestGuest::new();
        let handles: Vec<SocketHandle> = (0..N)
            .map(|i| {
                let h = g.socket(4096, 4096);
                g.connect(h, SocketAddrV4::new(GATEWAY_IP, port), 41000 + i as u16);
                h
            })
            .collect();
        let mut replies = vec![Vec::new(); N];
        let mut sent = vec![false; N];
        let finished = g.run(Duration::from_secs(30), |g| {
            let mut all_closed = true;
            for (i, &h) in handles.iter().enumerate() {
                let s = g.tcp(h);
                if !sent[i] && s.can_send() {
                    s.send_slice(format!("message {i}").as_bytes()).unwrap();
                    s.close();
                    sent[i] = true;
                }
                if s.can_recv() {
                    s.recv(|b| {
                        replies[i].extend_from_slice(b);
                        (b.len(), ())
                    })
                    .unwrap();
                }
                all_closed &= matches!(s.state(), stcp::State::TimeWait | stcp::State::Closed);
            }
            all_closed
        });
        assert!(finished, "timed out");
        for (i, reply) in replies.iter().enumerate() {
            let mut expected = format!("message {i}").into_bytes();
            expected.reverse();
            assert_eq!(reply, &expected, "connection {i}");
        }
        host.join().unwrap();
    }

    /// hostfwd: a host connection to the forwarded port reaches the guest.
    #[test]
    fn tcp_port_forward_into_the_guest() {
        let mut g = TestGuest::new();
        let (host_addr, guest_addr) = g.net.add_forward("tcp:127.0.0.1:0-:2222").unwrap();
        assert_eq!(guest_addr, SocketAddrV4::new(GUEST_IP, 2222));
        g.announce();
        let h = g.socket(4096, 4096);
        g.tcp(h).listen(2222).unwrap();

        let client = std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(host_addr).unwrap();
            stream.write_all(b"hello guest").unwrap();
            stream.shutdown(std::net::Shutdown::Write).unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            answer
        });
        let mut request = Vec::new();
        let mut answered = false;
        let finished = g.run(Duration::from_secs(20), |g| {
            let s = g.tcp(h);
            if s.can_recv() {
                s.recv(|b| {
                    request.extend_from_slice(b);
                    (b.len(), ())
                })
                .unwrap();
            }
            if !answered && s.state() == stcp::State::CloseWait && !s.can_recv() {
                s.send_slice(&request.to_ascii_uppercase()).unwrap();
                s.close();
                answered = true;
            }
            answered && s.state() == stcp::State::Closed
        });
        assert!(finished, "timed out; the guest received {:?}", String::from_utf8_lossy(&request));
        assert_eq!(client.join().unwrap(), "HELLO GUEST");
    }

    /// Run `body` on a thread of its own, failing if it takes longer than
    /// `timeout`: a network thread stuck in a loop would otherwise hang the
    /// test, as dropping the network waits for its thread.
    fn within(timeout: Duration, body: impl FnOnce() + Send + 'static) {
        use std::sync::mpsc::RecvTimeoutError;
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            body();
            let _ = done.send(());
        });
        match finished.recv_timeout(timeout) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => panic!("timed out after {timeout:?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("failed (see above)"),
        }
    }

    /// The host resets a connection while the guest is still sending: the
    /// guest's connection is reset too, and the network carries on.
    #[test]
    fn tcp_host_reset_while_guest_sends() {
        within(Duration::from_secs(60), || {
            let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = server.local_addr().unwrap().port();
            let host = std::thread::spawn(move || {
                let (mut stream, _) = server.accept().unwrap();
                let mut start = [0; 1000];
                stream.read_exact(&mut start).unwrap();
                // Close with a reset (SO_LINGER 0) rather than a FIN.
                socket2::SockRef::from(&stream).set_linger(Some(Duration::ZERO)).unwrap();
            });

            let mut g = TestGuest::new();
            let h = g.socket(64 * 1024, 64 * 1024);
            g.connect(h, SocketAddrV4::new(GATEWAY_IP, port), 40003);
            let data = pattern(4_000_000, 3);
            let mut sent = 0;
            let reset = g.run(Duration::from_secs(20), |g| {
                let s = g.tcp(h);
                if s.can_send() && sent < data.len() {
                    sent += s.send_slice(&data[sent..]).unwrap();
                }
                s.state() == stcp::State::Closed
            });
            host.join().unwrap();
            assert!(reset, "the guest's connection was not reset (it sent {sent} bytes)");

            // A new connection still works.
            let echo = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let echo_port = echo.local_addr().unwrap().port();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = echo.accept().unwrap();
                let mut message = Vec::new();
                stream.read_to_end(&mut message).unwrap();
                stream.write_all(&message).unwrap();
            });
            let h = g.socket(4096, 4096);
            g.connect(h, SocketAddrV4::new(GATEWAY_IP, echo_port), 40004);
            let (mut sent, mut reply) = (false, Vec::new());
            let finished = g.run(Duration::from_secs(20), |g| {
                let s = g.tcp(h);
                if !sent && s.can_send() {
                    s.send_slice(b"still working").unwrap();
                    s.close();
                    sent = true;
                }
                if s.can_recv() {
                    s.recv(|b| {
                        reply.extend_from_slice(b);
                        (b.len(), ())
                    })
                    .unwrap();
                }
                sent && matches!(s.state(), stcp::State::TimeWait | stcp::State::Closed)
            });
            assert!(finished && reply == b"still working", "got {:?}", String::from_utf8_lossy(&reply));
            server.join().unwrap();
        });
    }
}
