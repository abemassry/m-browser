//! TCP for QEMU's user network.
//!
//! A TCP connection from the guest ends in a TCP/IP stack on the host side
//! (smoltcp) that answers for every address: a SYN to 1.2.3.4:80 is taken
//! by a smoltcp socket posing as 1.2.3.4:80, and what the connection carries
//! is relayed through a host socket connected to the real 1.2.3.4:80. This
//! is how QEMU's slirp works too.
//!
//! The host connection is made first, and the guest's SYN is answered only
//! once it is up. So a refused or unreachable destination looks to the guest
//! as it would on a real network: a reset, or an ICMP error.
//!
//! Port forwarding (QEMU's hostfwd option) works the other way round: a host
//! socket accepts a connection, and smoltcp opens one to the guest, coming
//! from 10.0.2.2.
//!
//! Data moves between the two sockets of a connection only as far as the
//! buffers on both sides allow, so a slow reader on either side slows down
//! the writer on the other, through its TCP window.

use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4};
use std::time::Duration;

use mio::event::Event;
use mio::net::{TcpListener, TcpStream};
use mio::{Interest, Registry, Token};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Checksum, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};

use super::{in_virtual_network, GATEWAY_IP, GUEST_IP, MTU};

/// Receive and send buffers of each connection's smoltcp socket. The
/// receive buffer sets the TCP window offered to the guest.
const RX_BUFFER: usize = 128 * 1024;
const TX_BUFFER: usize = 128 * 1024;
/// At most this many connections at once; more are refused.
const MAX_CONNECTIONS: usize = 256;
/// A connection whose peer in the guest stops acknowledging data for this
/// long is dropped.
const GUEST_TIMEOUT: smoltcp::time::Duration = smoltcp::time::Duration::from_secs(300);
/// Source ports on 10.0.2.2 of forwarded connections into the guest.
const FORWARD_PORTS: std::ops::RangeInclusive<u16> = 49152..=65535;

const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const ACK: u8 = 0x10;

macro_rules! debug_log {
    ($on:expr, $($arg:tt)*) => {
        if $on {
            eprintln!("[qemu-net] {}", format_args!($($arg)*));
        }
    };
}

/// A TCP connection as the guest sees it: its own endpoint, and the one it
/// talks to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct Flow {
    pub guest: SocketAddrV4,
    pub remote: SocketAddrV4,
}

/// How to answer a segment from the guest.
pub(super) enum Verdict {
    /// It was taken care of.
    Done,
    /// With a reset: nothing listens there.
    Reset,
    /// With an ICMP "host unreachable".
    Unreachable,
}

/// A connection attempt from the guest that failed on the host side, to be
/// answered like `Verdict::Reset` or `Verdict::Unreachable`.
pub(super) struct Failed {
    /// The guest's SYN (its IPv4 packet).
    pub syn: Vec<u8>,
    pub refused: bool,
}

enum Phase {
    /// Waiting for the host connection; `syn` is the guest's latest SYN, to
    /// be answered once the connection is up.
    Connecting { syn: Vec<u8> },
    /// Relaying between the host socket and the smoltcp socket.
    Open { handle: SocketHandle },
}

struct Conn {
    flow: Flow,
    host: TcpStream,
    phase: Phase,
    /// mio reports readiness edge-triggered: these stay set until a read or
    /// write on the host socket would block.
    readable: bool,
    writable: bool,
    /// The host closed its sending side; the guest gets a FIN once the data
    /// before it has been sent.
    host_eof: bool,
    /// The guest closed its sending side, and so has the host socket.
    guest_eof: bool,
    /// The host connection failed with this error: the guest's connection is
    /// reset, and nothing more is relayed.
    failed: Option<String>,
    /// Made by a port forward, from the host into the guest.
    forwarded: bool,
    /// Bytes relayed, for the debug log.
    to_guest: u64,
    from_guest: u64,
}

/// A port forward: connections accepted by `listener` go to `guest`.
struct Forward {
    listener: TcpListener,
    guest: SocketAddrV4,
}

pub(super) struct TcpNat {
    iface: Interface,
    link: Link,
    sockets: SocketSet<'static>,
    /// Connections, by the token of their host socket.
    conns: HashMap<Token, Conn>,
    flows: HashMap<Flow, Token>,
    forwards: HashMap<Token, Forward>,
    next_forward_port: u16,
    /// The last `service` stopped before all work was done.
    more: bool,
    debug: bool,
    warned_limit: bool,
}

impl TcpNat {
    pub fn new(debug: bool) -> Self {
        let mut link = Link {
            rx: VecDeque::new(),
            tx: Vec::new(),
            room: usize::MAX,
            full: false,
        };
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = rand::random();
        let mut iface = Interface::new(config, &mut link, Instant::now());
        // Accept packets for every destination address (see the module
        // documentation). The interface's own address only matters for
        // routing: the guest's network is directly attached.
        iface.set_any_ip(true);
        iface.update_ip_addrs(|addrs| {
            addrs
                .push(IpCidr::new(IpAddress::Ipv4(GATEWAY_IP), 24))
                .expect("room for one address");
        });
        Self {
            iface,
            link,
            sockets: SocketSet::new(Vec::new()),
            conns: HashMap::new(),
            flows: HashMap::new(),
            forwards: HashMap::new(),
            next_forward_port: *FORWARD_PORTS.start(),
            more: false,
            debug,
            warned_limit: false,
        }
    }

    /// Handle a TCP segment from the guest; `packet` is its IPv4 packet,
    /// `flags` its TCP flags and `host` where its destination is on the
    /// host, if anywhere.
    pub fn from_guest(
        &mut self,
        packet: &[u8],
        flow: Flow,
        flags: u8,
        host: Option<SocketAddr>,
        registry: &Registry,
        next_token: &mut usize,
    ) -> Verdict {
        if let Some(&token) = self.flows.get(&flow) {
            let conn = self.conns.get_mut(&token).expect("flows and conns agree");
            match &mut conn.phase {
                // The guest repeats its SYN until it is answered.
                Phase::Connecting { syn } if flags & (SYN | ACK) == SYN => *syn = packet.to_vec(),
                Phase::Connecting { .. } if flags & RST != 0 => {
                    debug_log!(self.debug, "TCP {} -> {}: given up by the guest", flow.guest, flow.remote);
                    self.drop_conn(token, registry);
                }
                Phase::Connecting { .. } => {}
                Phase::Open { .. } => {
                    self.link.rx.push_back(packet.to_vec());
                    self.iface.poll_ingress_single(Instant::now(), &mut self.link, &mut self.sockets);
                }
            }
            return Verdict::Done;
        }

        // Not a connection we know: only a SYN may start one.
        if flags & (SYN | ACK | RST) != SYN {
            return if flags & RST != 0 { Verdict::Done } else { Verdict::Reset };
        }
        let Some(host) = host else {
            debug_log!(self.debug, "TCP {} -> {}: refused (no such host)", flow.guest, flow.remote);
            return Verdict::Reset;
        };
        if self.conns.len() >= MAX_CONNECTIONS {
            if !self.warned_limit {
                self.warned_limit = true;
                eprintln!("[qemu-net] more than {MAX_CONNECTIONS} TCP connections; refusing new ones");
            }
            return Verdict::Reset;
        }
        let mut stream = match TcpStream::connect(host) {
            Ok(stream) => stream,
            Err(e) => {
                debug_log!(self.debug, "TCP {} -> {} ({host}): {e}", flow.guest, flow.remote);
                return if e.kind() == ErrorKind::ConnectionRefused {
                    Verdict::Reset
                } else {
                    Verdict::Unreachable
                };
            }
        };
        let token = Token(*next_token);
        *next_token += 1;
        if let Err(e) = registry.register(&mut stream, token, Interest::READABLE | Interest::WRITABLE) {
            eprintln!("[qemu-net] cannot watch a host socket: {e}");
            return Verdict::Reset;
        }
        debug_log!(self.debug, "TCP {} -> {} ({host}): connecting", flow.guest, flow.remote);
        self.conns.insert(
            token,
            Conn {
                flow,
                host: stream,
                phase: Phase::Connecting { syn: packet.to_vec() },
                readable: false,
                writable: false,
                host_eof: false,
                guest_eof: false,
                failed: None,
                forwarded: false,
                to_guest: 0,
                from_guest: 0,
            },
        );
        self.flows.insert(flow, token);
        Verdict::Done
    }

    /// A host socket of ours is ready. Returns a connection attempt that
    /// failed, for the caller to report to the guest.
    pub fn host_event(&mut self, event: &Event, registry: &Registry, next_token: &mut usize) -> Option<Failed> {
        let token = event.token();
        if self.forwards.contains_key(&token) {
            self.accept_forwarded(token, registry, next_token);
            return None;
        }
        let conn = self.conns.get_mut(&token)?;
        // On an error or a hang-up, let reading and writing find out what
        // happened.
        let hangup = event.is_error() || event.is_read_closed() || event.is_write_closed();
        if event.is_readable() || hangup {
            conn.readable = true;
        }
        if event.is_writable() || hangup {
            conn.writable = true;
        }
        if !matches!(conn.phase, Phase::Connecting { .. }) || !conn.writable {
            return None;
        }
        match connect_result(&conn.host) {
            None => None,
            Some(Ok(())) => {
                self.answer_syn(token, registry);
                None
            }
            Some(Err(e)) => {
                let flow = conn.flow;
                debug_log!(self.debug, "TCP {} -> {}: {e}", flow.guest, flow.remote);
                let refused = e.kind() == ErrorKind::ConnectionRefused;
                let syn = match &mut conn.phase {
                    Phase::Connecting { syn } => std::mem::take(syn),
                    Phase::Open { .. } => unreachable!(),
                };
                self.drop_conn(token, registry);
                Some(Failed { syn, refused })
            }
        }
    }

    /// The host connection of a guest's connection attempt is up: let a
    /// smoltcp socket take the guest's SYN, which it answers.
    fn answer_syn(&mut self, token: Token, registry: &Registry) {
        let conn = self.conns.get_mut(&token).expect("connection exists");
        let syn = match &mut conn.phase {
            Phase::Connecting { syn } => std::mem::take(syn),
            Phase::Open { .. } => return,
        };
        let mut socket = new_socket();
        // A listening socket takes any SYN for its endpoint, so it is made
        // just for this SYN, which is processed at once. (Segments from the
        // guest are processed as they arrive, so none is waiting.)
        socket
            .listen(conn.flow.remote)
            .expect("a fresh socket can listen on a non-zero port");
        let handle = self.sockets.add(socket);
        conn.phase = Phase::Open { handle };
        conn.readable = true;
        conn.writable = true;
        self.link.rx.push_back(syn);
        self.iface.poll_ingress_single(Instant::now(), &mut self.link, &mut self.sockets);

        let flow = conn.flow;
        let socket = self.sockets.get::<tcp::Socket>(handle);
        if socket.state() != tcp::State::SynReceived {
            // Cannot happen: a SYN with these addresses started the attempt.
            eprintln!("[qemu-net] TCP {} -> {}: SYN not accepted", flow.guest, flow.remote);
            self.drop_conn(token, registry);
            return;
        }
        debug_log!(self.debug, "TCP {} -> {}: connected", flow.guest, flow.remote);
    }

    /// Start forwarding connections accepted by `listener` to `guest`.
    pub fn add_forward(&mut self, mut listener: TcpListener, guest: SocketAddrV4, registry: &Registry, next_token: &mut usize) {
        let token = Token(*next_token);
        *next_token += 1;
        if let Err(e) = registry.register(&mut listener, token, Interest::READABLE) {
            eprintln!("[qemu-net] cannot watch a host socket: {e}");
            return;
        }
        self.forwards.insert(token, Forward { listener, guest });
    }

    fn accept_forwarded(&mut self, token: Token, registry: &Registry, next_token: &mut usize) {
        loop {
            let Some(forward) = self.forwards.get(&token) else { return };
            let guest = forward.guest;
            let (mut stream, peer) = match forward.listener.accept() {
                Ok(accepted) => accepted,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("[qemu-net] port forward to {guest}: {e}");
                    return;
                }
            };
            if self.conns.len() >= MAX_CONNECTIONS {
                continue; // dropping the stream closes it
            }
            let Some(port) = self.free_forward_port(guest) else { continue };
            let flow = Flow {
                guest,
                remote: SocketAddrV4::new(GATEWAY_IP, port),
            };
            let mut socket = new_socket();
            if let Err(e) = socket.connect(self.iface.context(), flow.guest, flow.remote) {
                eprintln!("[qemu-net] port forward to {guest}: {e}");
                continue;
            }
            let conn_token = Token(*next_token);
            *next_token += 1;
            if let Err(e) = registry.register(&mut stream, conn_token, Interest::READABLE | Interest::WRITABLE) {
                eprintln!("[qemu-net] cannot watch a host socket: {e}");
                continue;
            }
            debug_log!(self.debug, "TCP {peer} -> {guest}: forwarded, from {}", flow.remote);
            let handle = self.sockets.add(socket);
            self.conns.insert(
                conn_token,
                Conn {
                    flow,
                    host: stream,
                    phase: Phase::Open { handle },
                    readable: true,
                    writable: true,
                    host_eof: false,
                    guest_eof: false,
                    failed: None,
                    forwarded: true,
                    to_guest: 0,
                    from_guest: 0,
                },
            );
            self.flows.insert(flow, conn_token);
        }
    }

    /// A port on 10.0.2.2 not in use by a connection to `guest`.
    fn free_forward_port(&mut self, guest: SocketAddrV4) -> Option<u16> {
        for _ in FORWARD_PORTS {
            let port = self.next_forward_port;
            self.next_forward_port = if port == *FORWARD_PORTS.end() { *FORWARD_PORTS.start() } else { port + 1 };
            let flow = Flow {
                guest,
                remote: SocketAddrV4::new(GATEWAY_IP, port),
            };
            if !self.flows.contains_key(&flow) {
                return Some(port);
            }
        }
        None
    }

    /// Run the TCP/IP stack and relay data until nothing moves any more.
    /// `room` is how many more packets the guest's queue takes for now.
    pub fn service(&mut self, room: usize, registry: &Registry) {
        // Each round moves at most a buffer's worth per connection, so this
        // ends after a few rounds. The cap is a safeguard: should a
        // connection ever keep reporting progress, the network thread still
        // gets back to its other work, and comes back here at once.
        const MAX_ROUNDS: usize = 64;
        self.link.room = room;
        self.link.full = false;
        self.more = false;
        for _ in 0..MAX_ROUNDS {
            self.iface.poll(Instant::now(), &mut self.link, &mut self.sockets);
            if !self.relay() {
                self.reap(registry);
                return;
            }
        }
        self.more = true;
        self.reap(registry);
    }

    /// IP packets for the guest.
    pub fn take_packets(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.link.tx)
    }

    /// Whether the stack held packets back for lack of room in the guest's
    /// queue during the last `service`.
    pub fn starved(&self) -> bool {
        self.link.full
    }

    /// When the stack needs to run again (for retransmissions, delayed
    /// acknowledgements and the like), if at all.
    pub fn poll_delay(&mut self) -> Option<Duration> {
        if self.more {
            return Some(Duration::ZERO);
        }
        self.iface
            .poll_delay(Instant::now(), &self.sockets)
            .map(|d| Duration::from_micros(d.total_micros()))
    }

    /// Move data between host and smoltcp sockets, in both directions, as
    /// far as their buffers allow. Returns whether anything changed.
    fn relay(&mut self) -> bool {
        let mut progress = false;
        for conn in self.conns.values_mut() {
            if let Phase::Open { handle } = conn.phase {
                progress |= relay(conn, self.sockets.get_mut::<tcp::Socket>(handle));
            }
        }
        progress
    }

    /// Forget connections that are over.
    fn reap(&mut self, registry: &Registry) {
        let over: Vec<Token> = self
            .conns
            .iter()
            .filter(|(_, conn)| match conn.phase {
                Phase::Open { handle } => {
                    let socket = self.sockets.get::<tcp::Socket>(handle);
                    match socket.state() {
                        // Closed, and no reset left to send to the guest.
                        tcp::State::Closed => socket.remote_endpoint().is_none(),
                        tcp::State::TimeWait => true,
                        _ => false,
                    }
                }
                Phase::Connecting { .. } => false,
            })
            .map(|(&token, _)| token)
            .collect();
        for token in over {
            self.drop_conn(token, registry);
        }
    }

    fn drop_conn(&mut self, token: Token, registry: &Registry) {
        let Some(mut conn) = self.conns.remove(&token) else { return };
        self.flows.remove(&conn.flow);
        if let Phase::Open { handle } = conn.phase {
            let state = self.sockets.get::<tcp::Socket>(handle).state();
            self.sockets.remove(handle);
            // A connection the guest reset is reset on the host side too.
            if state == tcp::State::Closed && !(conn.host_eof && conn.guest_eof) {
                let _ = socket2::SockRef::from(&conn.host).set_linger(Some(Duration::ZERO));
            }
        }
        let _ = registry.deregister(&mut conn.host);
        let flow = conn.flow;
        let (from, to) = if conn.forwarded { (flow.remote, flow.guest) } else { (flow.guest, flow.remote) };
        debug_log!(
            self.debug,
            "TCP {from} -> {to}: closed ({} bytes to the guest, {} from it){}",
            conn.to_guest,
            conn.from_guest,
            conn.failed.map_or(String::new(), |e| format!("; the host connection failed: {e}"))
        );
    }
}

/// A smoltcp socket for one connection.
fn new_socket() -> tcp::Socket<'static> {
    let mut socket = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; RX_BUFFER]),
        tcp::SocketBuffer::new(vec![0; TX_BUFFER]),
    );
    // Relay small writes (keystrokes in ssh, say) at once.
    socket.set_nagle_enabled(false);
    socket.set_timeout(Some(GUEST_TIMEOUT));
    socket
}

/// Relay one connection's data. Returns whether anything changed.
fn relay(conn: &mut Conn, socket: &mut tcp::Socket) -> bool {
    // A connection whose host side failed is left alone: its socket only
    // still has to send the guest a reset, and then it is reaped. (Any data
    // the guest sent is still in the socket, and must not be written again.)
    if conn.failed.is_some() {
        return false;
    }
    let mut progress = false;

    // Guest to host, as far as the host socket takes it.
    while conn.writable && socket.can_recv() {
        let written = socket.recv(|data| match conn.host.write(data) {
            Ok(n) => (n, Ok(n)),
            Err(e) => (0, Err(e)),
        });
        match written {
            Ok(Ok(0)) => return host_failed(conn, socket, "the host socket takes no more data".into()),
            Ok(Ok(n)) => {
                conn.from_guest += n as u64;
                progress = true;
            }
            Ok(Err(e)) if e.kind() == ErrorKind::WouldBlock => conn.writable = false,
            Ok(Err(e)) if e.kind() == ErrorKind::Interrupted => {}
            Ok(Err(e)) => return host_failed(conn, socket, e.to_string()),
            Err(_) => break,
        }
    }
    // The guest closed its side, and everything it sent was passed on.
    if !conn.guest_eof
        && !socket.can_recv()
        && matches!(
            socket.state(),
            tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closing | tcp::State::TimeWait
        )
    {
        let _ = conn.host.shutdown(Shutdown::Write);
        conn.guest_eof = true;
        progress = true;
    }

    // Host to guest, as far as the socket's send buffer takes it.
    while conn.readable && !conn.host_eof && socket.can_send() {
        let read = socket.send(|buf| {
            if buf.is_empty() {
                return (0, None);
            }
            match conn.host.read(buf) {
                Ok(n) => (n, Some(Ok(n))),
                Err(e) => (0, Some(Err(e))),
            }
        });
        match read {
            Ok(None) | Err(_) => break,
            Ok(Some(Ok(0))) => {
                // End of the host's data: a FIN follows the data queued.
                conn.host_eof = true;
                socket.close();
                // The guest's last segment, its FIN, will then be
                // acknowledged in TIME-WAIT, a state reaped right after the
                // next poll: acknowledge at once rather than after the usual
                // delay, or the guest would retransmit its FIN to a
                // connection that is gone.
                socket.set_ack_delay(None);
                progress = true;
            }
            Ok(Some(Ok(n))) => {
                conn.to_guest += n as u64;
                progress = true;
            }
            Ok(Some(Err(e))) if e.kind() == ErrorKind::WouldBlock => conn.readable = false,
            Ok(Some(Err(e))) if e.kind() == ErrorKind::Interrupted => {}
            Ok(Some(Err(e))) => return host_failed(conn, socket, e.to_string()),
        }
    }
    progress
}

/// The host connection failed: reset the guest's, and stop relaying.
fn host_failed(conn: &mut Conn, socket: &mut tcp::Socket, error: String) -> bool {
    conn.failed = Some(error);
    socket.abort();
    true
}

/// The outcome of a non-blocking connect: None while it is in progress.
fn connect_result(stream: &TcpStream) -> Option<io::Result<()>> {
    match stream.take_error() {
        Ok(Some(e)) | Err(e) => return Some(Err(e)),
        Ok(None) => {}
    }
    match stream.peer_addr() {
        Ok(_) => Some(Ok(())),
        Err(e) if e.kind() == ErrorKind::NotConnected => None,
        Err(e) => Some(Err(e)),
    }
}

/// Parse a QEMU port forwarding rule, `[tcp]:[hostaddr]:hostport-[guestaddr]:guestport`,
/// into the host address to listen on and the guest address to connect to.
pub(super) fn parse_hostfwd(rule: &str) -> Result<(SocketAddrV4, SocketAddrV4), String> {
    const FORMAT: &str = "expected [tcp]:[hostaddr]:hostport-[guestaddr]:guestport";
    let (protocol, rest) = rule.split_once(':').ok_or(FORMAT)?;
    match protocol {
        "" | "tcp" => {}
        "udp" => return Err("forwarding UDP is not supported yet".into()),
        _ => return Err(format!("unknown protocol '{protocol}'")),
    }
    let (host, guest) = rest.split_once('-').ok_or(FORMAT)?;
    let endpoint = |s: &str, default: Ipv4Addr| -> Result<SocketAddrV4, String> {
        let (addr, port) = s.rsplit_once(':').ok_or(FORMAT)?;
        let ip = if addr.is_empty() {
            default
        } else {
            addr.parse().map_err(|_| format!("bad IPv4 address '{addr}'"))?
        };
        let port = port.parse().map_err(|_| format!("bad port '{port}'"))?;
        Ok(SocketAddrV4::new(ip, port))
    };
    let host = endpoint(host, Ipv4Addr::UNSPECIFIED)?;
    let guest = endpoint(guest, GUEST_IP)?;
    if guest.port() == 0 {
        return Err("the guest port cannot be 0".into());
    }
    if !in_virtual_network(*guest.ip()) || *guest.ip() == GATEWAY_IP {
        return Err(format!("{} is not a guest address", guest.ip()));
    }
    Ok((host, guest))
}

// ---------------------------------------------------------------------------
//  The link to the guest, as smoltcp sees it: IP packets in and out
// ---------------------------------------------------------------------------

struct Link {
    rx: VecDeque<Vec<u8>>,
    tx: Vec<Vec<u8>>,
    /// How many more packets the guest's queue takes for now; smoltcp holds
    /// packets back (and retries later) when it runs out.
    room: usize,
    /// smoltcp was held back since `room` was last set.
    full: bool,
}

struct RxToken(Vec<u8>);

struct TxToken<'a>(&'a mut Link);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut packet = vec![0; len];
        let result = f(&mut packet);
        self.0.room = self.0.room.saturating_sub(1);
        self.0.tx.push(packet);
        result
    }
}

impl Device for Link {
    type RxToken<'a> = RxToken;
    type TxToken<'a> = TxToken<'a>;

    fn receive(&mut self, _now: Instant) -> Option<(RxToken, TxToken<'_>)> {
        let packet = self.rx.pop_front()?;
        Some((RxToken(packet), TxToken(self)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<TxToken<'_>> {
        if self.room == 0 {
            self.full = true;
            return None;
        }
        Some(TxToken(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = MTU;
        // Fill in checksums for the guest, but do not check the guest's, as
        // for the other protocols: frames cannot be damaged on this link.
        caps.checksum.ipv4 = Checksum::Tx;
        caps.checksum.tcp = Checksum::Tx;
        caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostfwd_rules() {
        let addr = |s: &str| s.parse::<SocketAddrV4>().unwrap();
        assert_eq!(parse_hostfwd("tcp::2222-:22"), Ok((addr("0.0.0.0:2222"), addr("10.0.2.15:22"))));
        assert_eq!(
            parse_hostfwd(":127.0.0.1:8080-10.0.2.15:80"),
            Ok((addr("127.0.0.1:8080"), addr("10.0.2.15:80")))
        );
        assert!(parse_hostfwd("udp::5353-:53").is_err());
        assert!(parse_hostfwd("tcp::2222").is_err());
        assert!(parse_hostfwd("tcp::2222-:0").is_err());
        assert!(parse_hostfwd("tcp::2222-192.168.1.5:22").is_err());
        assert!(parse_hostfwd("sctp::1-:2").is_err());
    }
}
