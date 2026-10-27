# The guest's network (src/qemu_net.rs, src/qemu_net/tcp.rs)

**What this covers.** How the Alpine guest reaches the network: QEMU's
WASI "user" network backend hands the guest's Ethernet frames to m-browser,
which runs a small virtual network on a thread of its own and relays the
traffic through ordinary host sockets. Read it before changing
`src/qemu_net.rs` or `src/qemu_net/tcp.rs`, when the guest has no network,
or to add a protocol or a forwarding feature. No knowledge of networking
internals is assumed: the primer explains the terms (Rust and wasmtime
terms are in [02-concepts.md](02-concepts.md)).

## Overview

```text
+-- guest (Alpine Linux) ----------------------------------------------+
| wget, apk, ping -> guest kernel's TCP/IP -> e1000 driver             |
+-- QEMU (qemu-system-x86_64.wasm in wasmtime) ------------------------+
| e1000 model <-> net/wasi-user.c.inc (the "-nic user" backend)        |
+------+---------------------------------------------^-----------------+
       | host_net_send(frame)                        | host_net_recv(buf)
       v any QEMU thread                             | main loop thread
+-- m-browser: VirtualNet (src/qemu_net.rs) ---------+-----------------+
| Shared.from_guest    (at most QUEUE_MAX frames each)  Shared.to_guest |
+------+---------------------------------------------^-----------------+
       v waker.wake()                                | send_frame()
+-- "qemu-net" thread: Stack::run, a mio event loop -+-----------------+
| answers itself: ARP, DHCP, pings to 10.0.2.2/.3, IPv4 reassembly     |
| relays: UDP and DNS (HostSocket), pings (ICMP socket), TCP (TcpNat)  |
+------+---------------------------------------------^-----------------+
       v send, connect, write                        | readable, accept
  host sockets: UDP, ICMP "ping" datagram, TCP streams, TCP listeners
  then the internet, the LAN, and the host's 127.0.0.1 (seen as 10.0.2.2)
```

A native QEMU implements `-nic user` with libslirp; the WASI build has
none, so its backend only passes frames across the wasm boundary, and
m-browser is the network at the other end of the cable. The `qemu-net`
thread's `Stack` plays every other machine on a small LAN: it answers ARP,
DHCP and pings to its own addresses, relays UDP (DNS included) and pings
through host sockets, and ends the guest's TCP connections in smoltcp (a
TCP/IP stack written in Rust), relaying their data through host TCP
sockets. On its own thread, it never holds up the emulation while waiting
for host sockets. History: ping, DNS and UDP came first, TCP and port
forwarding later; `wget`, `apk` package installs, `ping` and DNS are
verified in the guest.

## A networking primer

Data on a network is nested like envelopes: each layer puts a header in
front of the data of the layer above.

```text
| Ethernet header | IPv4 header       | TCP / UDP / ICMP header | data ... |
  14 bytes          20 bytes or more    20+ / 8 / 8 bytes
  dst MAC, src MAC  src IP, dst IP,     ports (TCP, UDP), sequence numbers
  EtherType         protocol, length,   and flags (TCP), type (ICMP)
                    id, fragment info
```

| Term | Meaning, and where it shows up here |
|---|---|
| Frame | The unit a network card sends: an Ethernet header plus a payload. QEMU and m-browser exchange whole frames (`Vec<u8>`) |
| Packet | An IP (Internet Protocol) header plus its payload; the payload of a frame whose EtherType is `ETH_IPV4` |
| Segment, datagram | The payload of an IP packet: a TCP *segment*, a UDP *datagram* or an ICMP *message*; the IP header's protocol byte says which (`IP_TCP`, `IP_UDP`, `IP_ICMP`) |
| MAC address | The 6-byte hardware address of a network card; it only matters on the local network |
| IP address, port | The 4-byte address of a machine anywhere (IPv4), and the 2-byte number that picks a program on it (TCP and UDP only) |
| ARP | Address Resolution Protocol: "who has IP 10.0.2.2?" broadcast on the LAN, answered with a MAC address; needed before sending a frame to an IP on the LAN |
| DHCP | Dynamic Host Configuration Protocol: how a machine that just booted gets its IP address, its gateway (the machine that forwards everything leaving the LAN: the "default route") and its DNS server |
| DNS | Domain Name System: turns names like `dl-cdn.alpinelinux.org` into IP addresses; a query and its answer are usually one UDP datagram each, to port 53 |
| UDP | User Datagram Protocol: independent datagrams, no connection, no delivery guarantee |
| TCP | Transmission Control Protocol: a reliable byte stream over a connection opened with a handshake (SYN, SYN-ACK, ACK), closed with FIN (graceful) or RST (reset, abrupt). Each side advertises a *window*: how much more data it can take; this is how a slow reader slows down a fast writer |
| ICMP, ping | Internet Control Message Protocol: error reports ("destination unreachable") and *echo request/reply*, which `ping` uses |
| NAT | Network Address Translation: rewriting addresses so that many machines share one; here the guest's traffic leaves through the host's own sockets, so the world sees the host |
| MTU | Maximum Transmission Unit: the largest IP packet a link carries, 1500 bytes on Ethernet (`MTU`) |
| Fragmentation | Splitting an IP packet larger than the MTU into pieces that the receiver *reassembles*; all pieces share the packet's id |
| Checksum | A 16-bit sum over a header (IP) or a whole segment (TCP, UDP, ICMP) that lets the receiver detect corruption; `checksum()` and `transport_checksum()` compute it |
| Big-endian | Multi-byte numbers in headers are sent most significant byte first ("network byte order"): hence the `u16::from_be_bytes` and `to_be_bytes` calls everywhere |

## The QEMU side in brief

The QEMU half (detailed in qemu-wasi's docs/09-display-input-network.md)
is `qemu-src/net/wasi-user.c.inc`, included from `qemu-src/net/net.c`,
whose backend table maps `NET_CLIENT_DRIVER_USER` (`-nic user`, `-netdev
user`) to `net_init_wasi_user` instead of slirp. Its five host imports are
registered by `add_net_to_linker` (called by `run_wasm_module`,
`src/wasm.rs`) and touch QEMU's memory only through `with_guest` and
`with_guest_mut` (`src/qemu_jit.rs`): the multi-threaded build's memory is
a wasmtime `SharedMemory` that other threads use meanwhile
([13-adding-features.md](13-adding-features.md), Part 1, step 4). Wasm
types: [reference/host-imports.md](reference/host-imports.md).

| Import (C prototype) | Called from | Rust side | Semantics |
|---|---|---|---|
| `int32_t host_net_send(const void *frame, int32_t len)` | `wasi_user_receive`, on whichever thread runs the NIC model: the main loop, or a vCPU thread in the multi-threaded build | `VirtualNet::from_guest` | Queue one frame from the guest. Returns `len` even if the frame was dropped (queue full), -1 if the buffer is out of bounds. QEMU ignores the result, like a wire that may lose frames |
| `int32_t host_net_recv(void *buf, int32_t size)` | `wasi_net_poll`, main loop thread | `VirtualNet::to_guest` | Copy the next frame for the guest into `buf` and return its length, or 0 if there is none. A frame longer than `size` is dropped (cannot happen: frames are at most 1514 bytes, QEMU's buffer is `NET_BUFSIZE`, 69632) |
| `void host_net_wait(int64_t timeout_ns)` | `wasi_user_idle_wait`, main loop thread | `VirtualNet::wait_for_frame` | Sleep up to `timeout_ns`; return early when a frame for the guest is queued or `host_main_wake` is called. The profiler counts it as idle time (`HostWork::IdleWait`) |
| `int32_t host_net_hostfwd(const char *rule, int32_t len)` | `net_init_wasi_user`, once per `hostfwd=` rule, when QEMU creates its network backends at start-up | `VirtualNet::add_forward` | Set up a port forward. Returns 0, or -1 after printing why the rule cannot be used |
| `void host_main_wake(void)` | `wasi_main_wake()` (`util/main-loop.c`), called by `aio_notify()` (`util/async.c`) on threads other than the main one; multi-threaded build only | `VirtualNet::wake_main` | End the main thread's current `host_net_wait`, or make the next one return at once |

**Pitfall:** `host_net_send` often runs on a vCPU thread holding QEMU's
big lock (BQL), so `VirtualNet::from_guest` only copies and queues the
frame. Never do real work or block there.

**Delivering frames.** Nothing wakes QEMU's main loop for this network, so
`qemu_main_loop_step()` (`qemu-src/system/runstate.c`) calls
`wasi_net_poll()` after every `main_loop_wait()`: it fetches frames with
`host_net_recv` until there are none and hands each to the NIC with
`qemu_send_packet_async`. When the NIC is full (the guest has not emptied
its receive ring), QEMU keeps that frame and sets the backend's `blocked`
flag, fetching nothing more until `wasi_user_send_completed` clears it.

**Sleeping.** `main_loop_wait()` (`util/main-loop.c`) sleeps through the
`wasi_idle_wait` hook, set to `wasi_user_idle_wait`, which calls
`host_net_wait` so that a frame for the guest ends the sleep at once
(while `blocked`, it uses `nanosleep`: waking for frames that cannot be
delivered would spin). The single-threaded build sleeps only when all
vCPUs are idle, at most 10 ms at a time; the multi-threaded one sleeps at
most 30 ms with the BQL released, and its other threads end the sleep with
`host_main_wake` when they need the main loop ([08-threads.md](08-threads.md)).

**Options.** `wasi_user_unsupported_option()` rejects what the fixed
network cannot honour: `hostname`, `restrict=on`, `ipv4=off`, `ipv6=on`,
`ip`, `net`, `host`, `tftp`, `bootfile`, `dhcpstart`, `dns`, `dnssearch`,
`domainname`, `ipv6-prefix`, `ipv6-prefixlen`, `ipv6-host`, `ipv6-dns`,
`smb`, `smbserver`, `guestfwd`, `tftp-server-name` ("user network option
'X' is not supported: ..."). `hostfwd` rules go verbatim to `host_net_hostfwd`
(a -1 gives "could not set up host forwarding rule"); a second user
network gives "only one user network is supported". These errors end QEMU
at start-up. NIC options such as `mac=` are fine.

## The host side

### VirtualNet, Shared and the qemu-net thread

`add_net_to_linker` creates one `VirtualNet` and moves an `Arc` of it into
each import closure. `VirtualNet::start` creates a mio `Poll` (mio is a
thin wrapper around Linux's epoll: one thread waits on many non-blocking
sockets), the `Shared` state, a `Stack`, and the `qemu-net` thread running
`Stack::run`. `Shared` holds everything both sides touch:

| Field | Set or used by | Read or waited on by | Purpose |
|---|---|---|---|
| `from_guest: Mutex<Vec<Vec<u8>>>` | `from_guest` (QEMU threads) | `Stack::run`, which takes the whole `Vec` | Frames from the guest. Full at `QUEUE_MAX`: new frames are dropped |
| `to_guest: Mutex<VecDeque<Vec<u8>>>` | `send_frame` (network thread) | `to_guest` (QEMU main loop) | Frames for the guest. Full at `QUEUE_MAX`: new frames are dropped |
| `to_guest_len: AtomicUsize` | both, while holding the `to_guest` lock | `to_guest` without the lock; `service_tcp` | Lets `host_net_recv`, called on every main loop iteration, return without locking when nothing is queued |
| `to_guest_ready: Condvar` | `send_frame` (`notify_one`), `wake_main` (`notify_all`) | `wait_for_frame` | Ends QEMU's idle sleep |
| `main_wake: AtomicBool` | `wake_main` | `wait_for_frame`, which clears it | "The main loop has work": set under the `to_guest` lock so a waiter cannot miss it |
| `wake_when_drained: AtomicBool` | `service_tcp`, when TCP was held back | `to_guest`, which wakes the network thread once the queue is below `QUEUE_MAX / 2` | Restarts TCP output as soon as the guest has made room |
| `new_forwards: Mutex<Vec<(TcpListener, SocketAddrV4)>>` | `add_forward` (QEMU's thread) | `Stack::run` | Hands bound hostfwd listeners to the network thread |
| `waker: mio::Waker` | `from_guest` (first frame only), `to_guest`, `add_forward`, `Drop` | `Stack::run` (token `WAKE`, 0) | Wakes the network thread's `poll` |
| `stop: AtomicBool` | `VirtualNet::drop` | `Stack::run` | Ends the thread |

`from_guest` wakes the network thread only when the queue goes from empty
to one frame: the thread empties the whole queue each time it wakes.

**Lifetime.** There is one network per Linker, living as long as the
Linker's closures. Store migrations (single-threaded build) instantiate
QEMU again with the same Linker, so the network and its connections
survive them
([06-qemu-runner.md](06-qemu-runner.md#store-migration-migrate_store));
the multi-threaded build's vCPU instances come from an `InstancePre` of
the same Linker. When the last reference goes, `VirtualNet::drop` sets
`stop`, wakes the thread and joins it, closing every host socket.

**Pitfall:** with the multi-threaded build the last reference goes only
when m-browser exits, because of a reference cycle
([known-issues.md](known-issues.md)). From reading the code, the old
network then keeps its forwarded host ports, so starting QEMU again in the
same session with the same `hostfwd` rule fails at start-up (`cannot
listen on ...: Address already in use (os error 98)`, then QEMU's `could
not set up host forwarding rule`). Restart m-browser instead.

### The event loop: Stack::run

Each iteration of `Stack::run`:

1. Computes the `poll` timeout: the shorter of 5 s (only while UDP/ping
   sockets or fragments exist, for the sweep) and smoltcp's next deadline
   (`TcpNat::poll_delay`: retransmissions, delayed ACKs, or 0 after a
   `service` that stopped early); 20 ms instead while TCP is starved
   ([Backpressure](#backpressure)); with none of these, no timeout.
2. Waits in `poll.poll` (at most 64 events at once; `EINTR` is retried,
   any other error ends the thread: "network thread stopped").
3. Dispatches each event by its mio *token* (the number a socket was
   registered with): `WAKE` does nothing, as the steps below run anyway; a
   token in `Stack.sockets` is a UDP or ping socket, for
   `socket_readable`; any other goes to `TcpNat::host_event` (connections
   and hostfwd listeners), which may return a failed connection attempt
   for `tcp_failed`.
4. Registers the listeners in `new_forwards` (`TcpNat::add_forward`).
5. Takes all frames in `from_guest` and handles each (`guest_frame`).
6. Runs TCP (`service_tcp`) and queues the packets it produced.
7. At most once a second: `close_idle_sockets`, and drops reassemblies
   older than `REASSEMBLY_TIMEOUT`.

**Pitfall:** tokens come from one counter, `Stack.next_token` (from 1; 0
is `WAKE`), shared with `TcpNat` through `&mut next_token`, and are never
reused. A new kind of socket must take its token from that counter and be
dispatched in step 3, or its events reach `TcpNat::host_event`, which
ignores them.

**Pitfall:** mio readiness is *edge-triggered*: an event says the socket
became readable, once. Its handler must read (or write) until the call
returns `WouldBlock`, or the rest waits forever. `socket_readable` loops;
TCP remembers readiness in `Conn.readable` and `Conn.writable`.

### Backpressure

No queue or buffer may grow without bound, so each stage stops taking
data while the next one is full:

```text
Download stages, in order           Each fills up while ...
host socket's kernel buffer         relay() stops reading: !can_send()
smoltcp send buffer (TX_BUFFER)     the guest's window is full, or
                                    Link::transmit refuses (room is 0)
Shared.to_guest (QUEUE_MAX)         QEMU does not fetch (`blocked`, busy)
e1000 receive ring                  the guest has not processed it

Upload stages, in order             Each fills up while ...
Shared.from_guest (QUEUE_MAX)       the network thread lags (then drops)
smoltcp receive buffer (RX_BUFFER)  host.write() would block; the window
                                    offered to the guest shrinks with it
host socket's kernel buffer         the remote reads slowly
```

`service_tcp` lets smoltcp send at most as many packets as `to_guest` has
room for (`QUEUE_MAX - to_guest_len`); beyond that, `Link::transmit`
refuses (smoltcp keeps the data for later) and sets `Link.full`.
`service_tcp` then sets `wake_when_drained`; `Stack::run` retries every
20 ms, and QEMU's `to_guest` wakes it as soon as the queue is below half.
UDP and pings have no flow control: a datagram finding a queue full is
dropped, as on a real network. A dropped TCP frame costs a retransmission.

## Addresses

The addresses are those of QEMU's slirp, so guests set up for it work:

| Address | Constant | What it is | Where it goes on the host |
|---|---|---|---|
| 10.0.2.0/24 | `NETMASK` 255.255.255.0 | The virtual LAN | |
| 10.0.2.15 | `GUEST_IP` | The guest, given by DHCP | |
| 10.0.2.2 | `GATEWAY_IP` | Gateway, DHCP server, client of forwarded connections | Port P is the host's 127.0.0.1:P |
| 10.0.2.3 | `DNS_IP` | DNS server | Port 53 only: the first `nameserver` of the host's /etc/resolv.conf |
| 10.0.2.255 | `BROADCAST_IP` | Broadcast address of the LAN | Nothing |
| other 10.0.2.x | | Nobody: ARP for them gets no answer | Nothing |
| any other unicast address | | The world | The same address (`is_routable`) |
| 52:55:0a:00:02:02 | `GATEWAY_MAC` | MAC of both 10.0.2.2 and 10.0.2.3 | |
| guest's MAC | `Stack.guest_mac` | Learned from the source of the guest's frames (QEMU's default is 52:54:00:12:34:56) | |
| 1500 bytes | `MTU` | Largest IP packet sent to the guest (larger ones are fragmented) | |

`Stack::host_address(addr)` computes the last column for UDP and TCP:
10.0.2.2 becomes 127.0.0.1; 10.0.2.3 port 53 becomes the nameserver; a
routable address (`is_routable`: unicast, outside 10.0.2.0/24) stays as it
is; anything else is `None`, answered with ICMP "port unreachable" (UDP)
or a reset (TCP). `host_nameserver()` reads the nameserver (the first
`nameserver` line that parses as an IP address) once, at start: a change
(VPN, another Wi-Fi) is seen by the next QEMU run only. IPv6 and loopback
nameservers such as systemd-resolved's 127.0.0.53 work, since the queries
leave from the host.

**Pitfall:** 10.0.2.2 is the host's IPv4 loopback: a host service on
127.0.0.1 is reachable from the guest (as with slirp), one listening only
on `::1` or on a LAN address is not. Nothing stops the guest from reaching
the host's LAN either; there is no `restrict` option.

## Frames and protocols

### Frames from the guest

`guest_frame` ignores frames under 14 bytes and learns the guest's MAC
from each frame's source address (unless it is a group address, which no
card uses as its own); the first frame prints `guest NIC <mac> is up`. ARP
goes to `arp`, IPv4 to `ipv4`, anything else (IPv6, VLAN tags) is dropped.
`ipv4` checks the version and lengths, cuts Ethernet padding, sends
fragments to `reassemble`, and `ipv4_packet` dispatches on the protocol to
`icmp`, `udp` or `tcp`. No checksum from the guest is verified: frames
cannot be damaged on this link. Everything for the guest goes through
`send_frame`, which pads it to Ethernet's 60-byte minimum and appends it
to `to_guest`. ARP and DHCP replies go to the MAC in the request; the rest
needs `guest_mac`, and is dropped until the guest has sent a frame.

### ARP

`arp` answers requests (Ethernet/IPv4, operation 1) for 10.0.2.2 and
10.0.2.3 only, both with `GATEWAY_MAC`: everything outside 10.0.2.0/24
goes through the gateway, so the guest needs no other MAC. Requests for
other addresses, the guest's own included (a duplicate address check), get
no answer, which correctly means "nobody has it". The network never sends
ARP requests: it knows the guest's MAC from its frames.

### DHCP

`udp` hands every datagram to port 67 to `dhcp`, whatever its destination
(a client without an address broadcasts). `dhcp` takes BOOTP requests with
the DHCP magic cookie, reads options 53 (type), 50 (requested address) and
54 (server) with `dhcp_options`, and answers:

| Guest sends | Condition | Answer |
|---|---|---|
| DISCOVER | | OFFER of 10.0.2.15 |
| REQUEST | option 54 names a server other than 10.0.2.2 | none |
| REQUEST | requested address (option 50, else the client's current address) is 10.0.2.15 | ACK |
| REQUEST | anything else | NAK |
| DECLINE, RELEASE, INFORM | | none |

Every reply has options 53 (type) and 54 (server 10.0.2.2); an OFFER or ACK
also offers 10.0.2.15 with options 51 (lease, `DHCP_LEASE_SECS` = 24 h), 1
(netmask), 3 (router 10.0.2.2), 6 (DNS server 10.0.2.3) and 28 (broadcast
10.0.2.255). It copies the request's transaction id, flags and hardware
address, is padded to 300 bytes (some clients insist on a full BOOTP
message) and goes from 10.0.2.2:67 to port 68 of the client's current
address, or of 255.255.255.255 if it has none. Renewals are simply
acknowledged; ACKs and NAKs are always logged (`DHCP: leased ...`).

### UDP relay and DNS

Each UDP *flow*, the pair (guest address and port, remote address and
port), gets its own host socket: `udp_from_guest` opens it for the first
datagram with `open_udp_socket`, bound to an ephemeral port and
*connected* to the host address (so the kernel delivers only datagrams
from there), and keeps it in `Stack.sockets` (a `HostSocket` with
`Flow::Udp`) and `Stack.udp_flows`. Replies arrive in `socket_readable`
and reach the guest through `send_udp`, from the address the guest sent
to (10.0.2.3:53, not the real nameserver), fragmented if needed.

DNS is not parsed at all: a query to 10.0.2.3:53 is a UDP flow whose host
address is the nameserver. The guest's resolver uses a new source port per
query, hence a new socket each time; these close after `DNS_IDLE_TIMEOUT`
(10 s) instead of `IDLE_TIMEOUT` (60 s). DNS over TCP to 10.0.2.3:53
reaches the same nameserver through the TCP NAT.

- The remote sees the host's address and a port the host kernel chose:
  the guest's source port is not preserved.
- `close_idle_sockets` closes sockets idle for their timeout, checked at
  the once-a-second sweep (the loop wakes at least every 5 s while
  sockets exist). A later datagram of the flow just opens a new socket.
- A datagram to a unicast address without a host address (another
  10.0.2.x, 10.0.2.3 on a port other than 53, or no nameserver) gets ICMP
  "port unreachable" (`send_unreachable`, code 3), so the guest's program
  fails at once instead of timing out. Broadcasts and multicasts (DHCP
  aside) are dropped.
- ICMP errors from outside are not relayed: on a connected socket they
  appear as receive errors, which `socket_readable` skips (at most 8 per
  wake-up). A datagram to a closed remote port goes unanswered.

### ICMP and ping

`icmp` handles echo requests (type 8) only. It answers those to 10.0.2.2
and 10.0.2.3 itself (`send_echo_reply`): `ping 10.0.2.2` tests the path
through QEMU to m-browser, not the host's network. For a routable address,
`ping_from_host` sends the request through an unprivileged ICMP socket
(`open_ping_socket`: a datagram socket for ICMP, which needs no root,
unlike the raw sockets of the classic `ping`), kept per (guest address,
ICMP identifier) in `Stack.pings`: in practice one per ping process in
the guest. The kernel sets its own identifier and checksum;
`socket_readable` turns each echo reply (type 0) back into one with the
guest's identifier. Replies reach the guest with TTL 64. Pings to other
10.0.2.x or broadcast addresses go unanswered.

**Pitfall:** Linux allows ICMP datagram sockets only to users with a group
in `net.ipv4.ping_group_range` ([Configuration](#configuration)). Without
it m-browser prints `cannot ping from the host: ...` once, and pings to
the internet time out; everything else works.

### IP fragments

`ipv4` passes each fragment from the guest (more-fragments flag or a
non-zero offset) to `reassemble`, which collects them in `Stack.fragments`
by `FragmentKey` (source, destination, protocol, id). Once the first
fragment (offset 0, with the header), the last (no more-fragments flag,
giving the length) and all between are in, in any order, the packet gets
the first header, with its length fixed and fragment fields cleared, and
goes to `ipv4_packet`. At most `MAX_REASSEMBLIES` (64) packets are
collected at once (fragments of others are dropped, with a debug line), a
packet over 65535 bytes is discarded, and incomplete ones go after
`REASSEMBLY_TIMEOUT` (30 s). The guest fragments big UDP datagrams and
pings (`ping -s 4000`), never TCP. Towards the guest, `send_ipv4_to`
splits payloads over 1480 bytes (the MTU less the 20-byte header, a
multiple of 8 as offsets require) into fragments sharing an id
(`Stack.ip_id`); smoltcp's TCP packets fit the MTU and need none.

### TCP: a NAT built on smoltcp

TCP cannot be relayed packet by packet: host sockets carry a byte stream,
not segments. So each guest connection ends in smoltcp on the network
thread, whose interface (address 10.0.2.2/24, `set_any_ip(true)`) accepts
packets for every destination: a SYN to 1.2.3.4:80 is taken by a smoltcp
socket posing as 1.2.3.4:80, and the data is relayed to a host socket
connected to the real 1.2.3.4:80, as QEMU's slirp does. smoltcp is built
with IPv4, TCP sockets and the "IP" medium only (`Cargo.toml`): `Link`
hands it bare IP packets, and `Stack` adds or strips the Ethernet header.

```text
guest 10.0.2.15:40000        qemu-net thread (TcpNat)               host
kernel TCP <==segments==> smoltcp socket     <--relay()-->  TcpStream to
                          "1.2.3.4:80" (TcpNat.sockets)     1.2.3.4:80
```

| Type (tcp.rs) | Role |
|---|---|
| `TcpNat` | The NAT: smoltcp's `Interface` and `SocketSet`, the `Link`, connections by host-socket token (`conns`) and by `Flow` (`flows`), hostfwd listeners (`forwards`) |
| `Flow` | A connection as the guest sees it: `guest` and `remote` endpoints (for a forward, `remote` is 10.0.2.2:port) |
| `Conn` | One connection: its `Flow`, host `TcpStream`, `Phase`, readiness flags, `host_eof`/`guest_eof`, `failed`, `forwarded`, byte counters for the log |
| `Phase` | `Connecting { syn }`: waiting for the host connection, holding the guest's latest SYN. `Open { handle }`: relaying, with a smoltcp socket |
| `Verdict`, `Failed` | How `Stack::tcp` must answer a segment (`Done`, `Reset` or `Unreachable`), and a connection attempt that failed later (the guest's SYN, and whether it was refused) |
| `Link` | smoltcp's "device": `rx` (packets from the guest), `tx` (packets for the guest), `room` and `full` for backpressure. It declares checksums as `Checksum::Tx`: smoltcp fills them in but does not check the guest's |

**Opening.** `Stack::tcp` builds the `Flow` and calls `TcpNat::from_guest`
with the destination's host address:

1. A segment of a known flow: in `Connecting`, a repeated SYN replaces the
   stored one and a RST (the guest gave up) drops the attempt; in `Open`,
   the packet goes into `Link.rx` and smoltcp processes it at once
   (`poll_ingress_single`).
2. An unknown flow that is not a plain SYN gets a reset (a stray RST is
   ignored), as from a host that has no such connection.
3. No host address, or `MAX_CONNECTIONS` (256) reached: a reset (the limit
   also prints a warning, once).
4. Otherwise a non-blocking `TcpStream::connect`: an immediate error gives
   `Reset` if refused, else `Unreachable`; on success the `Conn` starts in
   `Phase::Connecting`, and the guest gets no answer yet.
5. When the connect finishes, mio reports the stream writable and
   `host_event` asks `connect_result()`. On failure, `Stack::tcp_failed`
   answers the stored SYN with a RST if refused, else ICMP "host
   unreachable" (`send_unreachable`, code 1). On success, `answer_syn`
   makes a smoltcp socket listening on exactly `flow.remote` and feeds it
   the SYN: it moves to SYN-RECEIVED, and its SYN-ACK goes out at the next
   `service`.

Connecting first lets the guest see what a real network would show:
answering the SYN at once would make every refused or unreachable
destination look like a connection that opens and then breaks.

**Note:** a listening smoltcp socket takes any SYN for its endpoint, so
`answer_syn` makes one per SYN and feeds it that SYN at once; connections
to the same remote each get their own socket. A `Connecting` entry ends
only through the host connect (the host kernel gives up after about two
minutes without an answer, standard Linux behaviour) or a RST from the
guest, and counts towards `MAX_CONNECTIONS` meanwhile.

**Pitfall:** every TCP test connects to 10.0.2.2, the interface's own
address. Other destinations rely on `set_any_ip(true)` and are exercised
only by real use (`wget`, `apk`). smoltcp's documentation of `set_any_ip`
mentions routes, which this code does not add: after a smoltcp upgrade,
check a download from the internet by hand.

**Relaying.** `TcpNat::service(room)` runs up to `MAX_ROUNDS` (64) rounds
of `iface.poll` (smoltcp handles timers and sends what it can into
`Link.tx`) followed by `relay()` on each open connection, and stops at the
first round in which nothing moved. `relay(conn, socket)` moves data both
ways as far as the buffers allow:

- Guest to host: while the host socket is writable and the smoltcp socket
  holds data, write it (`socket.recv` + `host.write`). Once the guest has
  closed its side (CLOSE-WAIT, LAST-ACK, CLOSING or TIME-WAIT) and all its
  data is written, `shutdown(Write)` passes the FIN on (`guest_eof`).
- Host to guest: while the host socket is readable and the smoltcp send
  buffer has room, read into it (`socket.send` + `host.read`). A read of 0
  bytes is the host's FIN: `host_eof`, `socket.close()` (a FIN after the
  queued data), and `set_ack_delay(None)`, so that the guest's last FIN is
  acknowledged at once, before the connection is reaped.

`RX_BUFFER` (128 KiB) is the window offered to the guest; `TX_BUFFER`
(128 KiB) holds host data the guest has not acknowledged. Nagle's
algorithm (which delays small writes to merge them) is off, so ssh
keystrokes go out at once. A round moves at most a buffer's worth per
connection, so `service` usually ends after a few rounds; if it uses all
64, it sets `TcpNat.more`, `poll_delay` returns 0, and the thread handles
its other events and frames before coming back.

**Closing.** `reap`, at the end of every `service`, drops each open
connection whose smoltcp socket is in TIME-WAIT, or CLOSED with no remote
endpoint left (an aborted socket keeps it until its RST has gone out).
`drop_conn` removes the socket, deregisters and closes the host stream and
logs the byte counts. If the socket ended CLOSED without both sides
finishing cleanly (the guest reset it, or `GUEST_TIMEOUT` aborted it), it
first sets `SO_LINGER` to 0 on the host socket, so that closing it sends a
RST: the host peer sees a reset, not a clean end of data.

**Host failures.** When reading or writing the host socket fails (the
remote reset the connection, say), `host_failed` stores the error in
`conn.failed` and aborts the smoltcp socket, which sends the guest a RST;
`relay` skips the connection from then on, and `reap` removes it once the
RST is out. History: before `failed` existed, a host-side reset while the
guest kept sending made the network thread loop forever, because the
guest's unread data stayed in the aborted socket and every round tried to
write it again, reporting progress. The fix was `conn.failed` and
`host_failed`, the `MAX_ROUNDS` cap with `more` as a safeguard against any
similar loop, and the regression test `tcp_host_reset_while_guest_sends`.

**Pitfall:** `relay` must report progress only when something really
changed; anything that reports it on every call brings the loop back,
merely capped at 64 rounds per wake-up.

**Timeouts.** `GUEST_TIMEOUT` (300 s) is smoltcp's socket timeout: a
connection is aborted when the guest has not answered for that long while
data waits for it (or, for a forward, while its SYN is unanswered). There
is no idle timeout and no keep-alive: an idle connection stays open until
one side closes it.

### Port forwarding (hostfwd)

A forward works the other way round: a host listener accepts a
connection, and smoltcp opens one to the guest, coming from 10.0.2.2.
`tcp::parse_hostfwd` reads QEMU's syntax,
`[tcp]:[hostaddr]:hostport-[guestaddr]:guestport`:

| Rule | Listens on | Forwards to |
|---|---|---|
| `tcp:127.0.0.1:2222-:22` | 127.0.0.1:2222 | 10.0.2.15:22 |
| `tcp::2222-:22` | 0.0.0.0:2222: every interface, the LAN included | 10.0.2.15:22 |
| `:127.0.0.1:8080-10.0.2.15:80` | 127.0.0.1:8080 | 10.0.2.15:80 |
| `tcp:127.0.0.1:0-:80` | a free port, printed at start-up | 10.0.2.15:80 |

It refuses, with a message saying why, `udp` ("forwarding UDP is not
supported yet") and other protocols, a rule without `-`, a host name
instead of a numeric IPv4 address, a bad port, guest port 0, and a guest
address outside 10.0.2.0/24 or equal to 10.0.2.2 (only 10.0.2.15 is
useful).

1. At start-up `VirtualNet::add_forward` parses the rule and binds the
   listener at once, on QEMU's thread, so that "address already in use" or
   "permission denied" reaches QEMU as -1 and stops it with a clear
   message (`hostfwd=<rule>: cannot listen on ...`). On success it prints
   `forwarding <host> to the guest's <guest>`.
2. The listener travels through `Shared.new_forwards` to `Stack::run`,
   whose `TcpNat::add_forward` registers it in `TcpNat.forwards`. Clients
   that connect earlier wait in the kernel's backlog.
3. When it is readable, `host_event` calls `accept_forwarded`, which
   accepts until `WouldBlock`, closing connections beyond
   `MAX_CONNECTIONS` at once. `free_forward_port` takes the next port of
   `FORWARD_PORTS` (49152-65535, round-robin, skipping any that a current
   connection to the same guest endpoint uses), and a smoltcp socket
   connects from 10.0.2.2:port to the guest; the `Conn` starts in
   `Phase::Open` with `forwarded` set.

The guest sees every forwarded client as 10.0.2.2 (`sshd` logs say so).
If nothing listens on the guest port, the guest answers with a RST, the
connection is reaped and the host client gets a reset. Forwarding is TCP
only, and set up at start-up only.

## What is not supported

| Missing | What happens instead | Notes |
|---|---|---|
| IPv6 | Frames ignored | The guest has only a link-local IPv6 address, so programs use IPv4 |
| UDP port forwarding | `parse_hostfwd` rejects `udp:` | Worked example: [13-adding-features.md](13-adding-features.md#part-3-worked-example-udp-port-forwarding) |
| Adding forwards at run time | Monitor commands `hostfwd_add`/`hostfwd_remove` do not exist | They are built only with slirp (`CONFIG_SLIRP`) |
| ICMP errors from outside | Not relayed | `traceroute` does not work; UDP to a closed remote port times out |
| Broadcast and multicast (except DHCP) | Dropped | No mDNS or service discovery |
| Other machines on the LAN | No ARP answers for them | One guest, one user network; no guest-to-guest traffic |
| `restrict`, `tftp`, `bootfile`, `smb`, `guestfwd`, custom `net`/`dns`/`dhcpstart`, ... | QEMU refuses to start | `wasi_user_unsupported_option` |
| Other IP protocols, ICMP other than echo | Dropped | |
| Original source port (UDP) and client address (hostfwd) | Replaced by the host's port, and by 10.0.2.2 | As with slirp |
| TTL handling | Not decremented; replies have TTL 64 | |

## Constants and limits

| Name | Value | File | Meaning |
|---|---|---|---|
| `GUEST_IP`, `GATEWAY_IP`, `DNS_IP`, `NETMASK`, `BROADCAST_IP`, `GATEWAY_MAC` | see [Addresses](#addresses) | qemu_net.rs | The virtual LAN |
| `DHCP_LEASE_SECS` | 86400 (24 h) | qemu_net.rs | Lease time offered |
| `MTU` | 1500 | qemu_net.rs | Largest IP packet to the guest; smoltcp's MTU |
| `QUEUE_MAX` | 1024 frames | qemu_net.rs | Capacity of `from_guest` and `to_guest` |
| `QUEUE_MAX / 2` | 512 frames | qemu_net.rs | `to_guest` level below which a starved TCP is woken |
| `IDLE_TIMEOUT` | 60 s | qemu_net.rs | Idle UDP and ping sockets are closed |
| `DNS_IDLE_TIMEOUT` | 10 s | qemu_net.rs | Same, for flows to 10.0.2.3 |
| `MAX_REASSEMBLIES` | 64 | qemu_net.rs | Packets reassembled at once |
| `REASSEMBLY_TIMEOUT` | 30 s | qemu_net.rs | Incomplete packets are dropped |
| `WAKE` | `Token(0)` | qemu_net.rs | The waker's token |
| loop timeouts (literals in `Stack::run`) | 5 s, 20 ms, 1 s | qemu_net.rs | Wake-up while sockets/fragments exist; retry while TCP is starved; sweep interval |
| `RX_BUFFER`, `TX_BUFFER` | 128 KiB each | tcp.rs | smoltcp socket buffers; RX sets the guest's window |
| `MAX_CONNECTIONS` | 256 | tcp.rs | TCP connections at once, all phases, forwards included |
| `GUEST_TIMEOUT` | 300 s | tcp.rs | smoltcp timeout for an unresponsive guest |
| `FORWARD_PORTS` | 49152..=65535 | tcp.rs | Source ports on 10.0.2.2 of forwarded connections |
| `MAX_ROUNDS` (in `service`) | 64 | tcp.rs | poll + relay rounds per `service` |
| idle wait cap (QEMU) | 10 ms / 30 ms | util/main-loop.c | Longest `host_net_wait`, single-/multi-threaded |

## Step by step

**(a) UDP out and back:** 10.0.2.15:5000 sends to 10.0.2.2:7777, where a
host program listens on 127.0.0.1:7777.

```text
QEMU      wasi_user_receive -> host_net_send -> VirtualNet::from_guest
qemu-net  Stack::run -> guest_frame -> ipv4 -> ipv4_packet -> udp
          -> host_address = 127.0.0.1:7777 -> udp_from_guest (new flow:
          open_udp_socket, add_socket) -> socket.send()
answer    socket event -> socket_readable -> send_udp (from 10.0.2.2:7777)
          -> send_udp_to -> send_ipv4_to -> send_frame -> Shared.to_guest
QEMU      (host_net_wait returns, if it slept) -> wasi_net_poll
          -> host_net_recv -> VirtualNet::to_guest -> qemu_send_packet_async
later     60 s without traffic: close_idle_sockets closes the socket
```

**(b) A DNS query** is (a) with three differences: the guest first asks
who has 10.0.2.3 (its nameserver, from DHCP option 6) and `arp` answers;
`host_address` maps 10.0.2.3:53 to the host's nameserver; and the socket
closes after `DNS_IDLE_TIMEOUT` (10 s).

**(c) An outbound TCP connection** (`wget http://example.com/`, after (b)
and an ARP request for the gateway 10.0.2.2):

```text
SYN       Stack::tcp -> TcpNat::from_guest -> TcpStream::connect
          (Phase::Connecting; immediate failure: Verdict::Reset/Unreachable)
connect   event -> host_event -> connect_result(): failed -> Failed ->
          Stack::tcp_failed (RST or ICMP unreachable); done -> answer_syn
          -> service_tcp -> service -> iface.poll: SYN-ACK -> send_ip_packet
data      guest's: from_guest -> Link.rx; relay(): socket.recv -> host.write
          host's: event -> host_event; relay(): host.read -> socket.send
close     host.read() == 0 -> host_eof, socket.close(); the guest's FIN ->
          relay(): shutdown(Write); TIME-WAIT -> reap -> drop_conn
```

**(d) An inbound forward** (`hostfwd=tcp:127.0.0.1:2222-:22`, then `ssh -p
2222 user@127.0.0.1`): at start-up `host_net_hostfwd` -> `add_forward`
(`parse_hostfwd`, bind) -> `new_forwards` -> `TcpNat::add_forward`; on
connection, listener event -> `host_event` -> `accept_forwarded` ->
`free_forward_port` (say 49152) -> a smoltcp socket connects from
10.0.2.2:49152 to 10.0.2.15:22, `service` sends its SYN, sshd answers, and
the data flows as in (c).

**(e) A ping** to 1.1.1.1: `icmp` -> `ping_from_host` (the first request
of this identifier: `open_ping_socket`, `add_socket`, `Stack.pings`) ->
`send_to`; the reply: socket event -> `socket_readable` ->
`send_echo_reply` -> `send_ipv4`. A ping to 10.0.2.2 stops in `icmp`,
which calls `send_echo_reply` at once.

## Configuration

The GUI starts QEMU with `-nic user`; the `QEMU_NIC` environment variable
replaces that value (read in `src/app.rs`; see
[reference/environment-variables.md](reference/environment-variables.md)).
To reach the guest's ssh server from the host:

```sh
# from the m-browser directory
QEMU_NIC=user,hostfwd=tcp:127.0.0.1:2222-:22 target/release/m-browser
# then, from any directory, once sshd runs in the guest:
ssh -p 2222 user@127.0.0.1
```

Repeat `hostfwd=` for several forwards, as in
`QEMU_NIC=user,hostfwd=tcp:127.0.0.1:2222-:22,hostfwd=tcp:127.0.0.1:8080-:80`,
and prefer 127.0.0.1 as the host address: an empty one exposes the guest's
port to the LAN. The headless runner
([11-headless-and-automation.md](11-headless-and-automation.md)) ignores
`QEMU_NIC`: give `-nic user,hostfwd=...` among QEMU's arguments instead.

**Host requirements.** UDP and TCP use ordinary sockets of the m-browser
process and need nothing special (a forwarded port below 1024 needs
privileges, as for any program). Pings to the internet need ICMP datagram
sockets, which Linux allows only to processes with a group (primary or
supplementary) inside `net.ipv4.ping_group_range`. This is standard Linux
configuration: the kernel's default, "1 0", allows nobody, and many
distributions widen it.

```sh
# any directory
sysctl net.ipv4.ping_group_range     # the allowed range of group ids
id -G                                # your group ids
sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"  # until reboot
echo 'net.ipv4.ping_group_range = 0 2147483647' |
    sudo tee /etc/sysctl.d/90-ping-group-range.conf     # at every boot
```

## Debugging

**QEMU_NET_DEBUG.** `Stack::new` turns the debug log on if the variable
exists, whatever its value: `QEMU_NET_DEBUG=0` and an empty value enable
it too. It is read when the network starts; `TcpNat::new` gets the same
flag. All lines go to stderr, prefixed with `[qemu-net]`:

| Topic | Debug lines (examples) |
|---|---|
| ARP, DHCP | `ARP: 10.0.2.2 is at 52:55:0a:00:02:02`, `DHCP: offered 10.0.2.15` |
| Ping | `ping 1.1.1.1 (id 7, seq 1)`, `ping reply from 1.1.1.1 (seq 1)`, `ping <addr> from the host: <error>` |
| UDP | `UDP 10.0.2.15:41000 -> 10.0.2.3:53 (127.0.0.53:53): 40 bytes`, `UDP 10.0.2.3:53 -> 10.0.2.15:41000: 56 bytes`, `UDP to <remote>: port unreachable`, `UDP to <host>: <error>` |
| Fragments | `dropped an IP fragment: too many packets in pieces` |
| TCP | `TCP <guest> -> <remote> (<host>): connecting`, `TCP <guest> -> <remote>: connected`, `...: refused (no such host)`, `...: given up by the guest`, `TCP <guest> -> <remote>: <error>`, `TCP connection to <addr>:<port> refused`, `TCP <client> -> <guest>: forwarded, from 10.0.2.2:<port>`, `TCP <from> -> <to>: closed (N bytes to the guest, M from it)`, plus `; the host connection failed: <error>` when it did |

Always printed: `guest NIC <mac> is up; DNS queries go to <addr>` (the
address reads `nowhere (no nameserver in /etc/resolv.conf)` without one),
`DHCP: leased ...`, `DHCP: refused ...`, `forwarding <host> to the guest's
<guest>`, `hostfwd=<rule>: <reason>`, and once each `cannot ping from the
host: ...` and `more than 256 TCP connections; refusing new ones`. Never
expected: `cannot watch a host socket`, `network thread stopped`, `SYN not
accepted`, `port forward to <guest>: <error>`.

**Inside the guest:**

```sh
# in the guest's shell
ip addr                  # 10.0.2.15/24 on the e1000 interface?
ip route                 # default via 10.0.2.2
cat /etc/resolv.conf     # nameserver 10.0.2.3
ping -c 3 10.0.2.2       # answered by m-browser itself
ping -c 3 1.1.1.1        # needs ping_group_range on the host
wget -O /dev/null http://example.com/
```

No 10.0.2.15: DHCP failed (is there a `DHCP: leased` line? does the guest
run its DHCP client?). `ping 10.0.2.2` fails: frames do not get through
QEMU at all. Only pings to the internet fail: ping sockets (see above);
`wget` does not need them. Names do not resolve: check the `DNS queries go
to` line and the host's /etc/resolv.conf. In QEMU's monitor (Ctrl+Alt+2
in the GUI, Ctrl+Alt+1 to return), `info network` lists the NIC and the
backend, "net=10.0.2.0/24, provided by the WASI host"
([14-testing-and-debugging.md](14-testing-and-debugging.md)).

## Tests

All 11 tests run offline, on the host's loopback only. The TCP tests need
no QEMU: a guest made of smoltcp (`TestGuest`; its `GuestNic` wraps IP
packets in Ethernet frames) talks through a real, running `VirtualNet` to
sockets on 127.0.0.1, reached as 10.0.2.2.

| Test | Checks |
|---|---|
| `qemu_net::tests::reassembles_fragments_in_any_order` | Three fragments arriving out of order give back the packet, with fixed length and cleared fragment fields |
| `qemu_net::tests::fragments_to_the_guest_reassemble` | `send_ipv4_to` splits 4000 bytes into 3 fragments within the MTU, with valid header checksums, that reassemble |
| `qemu_net::tests::checksum_matches_rfc1071_example` | `checksum` against RFC 1071's example, also split at an odd offset |
| `qemu_net::tests::dhcp_options_stop_at_end_or_truncation` | `dhcp_options` stops at the end option and at a truncated option |
| `qemu_net::tests::tcp_relays_both_ways` | 1 MB to a host echo server and back, then a close the host sees |
| `qemu_net::tests::tcp_refused_connection_is_reset` | A connection to a closed port is reset, never established |
| `qemu_net::tests::tcp_half_close_from_the_host` | The host sends 300 KB and closes; a slow guest reader with a small window gets all of it and the FIN, then answers |
| `qemu_net::tests::tcp_parallel_connections` | 20 connections to one host port at once, each with its own host connection |
| `qemu_net::tests::tcp_port_forward_into_the_guest` | `add_forward("tcp:127.0.0.1:0-:2222")`: a host client reaches a listener in the guest and gets its answer |
| `qemu_net::tests::tcp_host_reset_while_guest_sends` | The host resets while the guest sends 4 MB: the guest's connection is reset, and a new one still works (run under `within`, so a stuck network thread fails the test instead of hanging it) |
| `qemu_net::tcp::tests::hostfwd_rules` | `parse_hostfwd` accepts valid rules and refuses UDP, unknown protocols, a missing guest part, guest port 0 and non-guest addresses |

```sh
# from the m-browser directory
cargo test --release --offline qemu_net
# one test, with its output and the network's debug log
QEMU_NET_DEBUG=1 cargo test --release --offline \
    qemu_net::tests::tcp_relays_both_ways -- --nocapture
```

Not covered: ARP, a whole DHCP exchange, the UDP and DNS relays, pings
(they would need `ping_group_range`), ICMP errors sent to the guest,
unreachable (rather than refused) hosts, TCP to addresses other than
10.0.2.2, `MAX_CONNECTIONS`, `GUEST_TIMEOUT`, idle timeouts, the queue
limits and `wake_when_drained`, the host imports and QEMU's side; and a
Linux guest's TCP behaves differently from smoltcp's. After a change,
check by hand in the guest as in [Debugging](#debugging): DHCP, DNS,
`ping`, `wget` of a large file, an `apk add`. More on testing:
[14-testing-and-debugging.md](14-testing-and-debugging.md).
