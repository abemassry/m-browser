# Adding features

**What this covers.** How to add a feature that spans m-browser and the
WebAssembly build of QEMU: a new function that QEMU calls in m-browser (a
*host import*), from the C declaration in qemu-wasi to the Rust code,
tests and documentation; a new environment-variable setting; a complete
design for forwarding UDP ports into the guest, as a worked example; and
pointers for smaller changes. Read it before your first change that
involves QEMU, after [02-concepts.md](02-concepts.md) (Store, Linker,
linear memory). QEMU-side recipes are in the qemu-wasi repository's
docs/12-adding-features.md.

## Where the code of a feature goes

QEMU runs as a sandboxed wasm module: it reaches the outside world only
through functions m-browser hands it at instantiation. So most features
have a C half in qemu-wasi and a Rust half here.

```text
qemu-wasi, net/wasi-user.c.inc             m-browser, src/qemu_net.rs
extern int32_t host_net_hostfwd(           add_net_to_linker(), called by
    const char *rule, int32_t len);        run_wasm_module (src/wasm.rs):
host_net_hostfwd(rule, strlen(rule)); -->    linker.func_wrap("env",
  (defined nowhere: an import from env)        "host_net_hostfwd", ...)
```

| Feature | qemu-wasi | m-browser |
|---|---|---|
| A service QEMU asks the host for | `extern` declaration and call, in the patch of its topic | `func_wrap` in the area's `add_*_to_linker` |
| Display, input | `display/wasi_gfx_display.c` (plain qemu-wasi code, not a patch) | `src/qemu_gfx.rs`, `src/app.rs`, `src/headless.rs` |
| Network behaviour | usually nothing: `net/wasi-user.c.inc` only passes frames and rules | `src/qemu_net.rs`, `src/qemu_net/tcp.rs` |
| JIT | `tcg/wasm32*`, `tcg/wasi_jit.h` (patch 0002) | `src/qemu_jit.rs` |
| A tuning knob | `getenv()` | `std::env::var()` |
| QEMU's arguments, menus | nothing | `src/app.rs` |

## Part 1: a new host import, end to end

A *host import* is a function a wasm module needs from outside, named by
a module and a field (`env`, `host_net_send`); m-browser registers a Rust
closure, the *host function*, under that name. Here it is called
`host_x`; a complete example follows the steps.

### 1. Declare and call it in QEMU

QEMU is linked with `-Wl,--allow-undefined` (qemu-wasi's
`scripts/fix_build_ninja.py`): a function declared but defined nowhere
becomes an import from module `env` under its C name. In `qemu-src/`:

- Declare `extern int32_t host_x(...);` next to its callers, as
  `net/wasi-user.c.inc` does, or in a header (`tcg/wasi_jit.h`), with a
  comment: what the host does, what the result means, where m-browser
  implements it. Name it after its area (`host_gfx_*`, `host_net_*`).
  Parameters: numbers and pointers only (step 6); strings and buffers as
  a pointer and a length.
- In code that native builds compile too, call it under
  `#ifdef WASI_PLATFORM` (`WASI_THREADS`: multi-threaded build only):
  `make native` builds the same tree for the instruction test, without
  `--allow-undefined`, and would fail to link.

**Pitfall:** a misspelt name, or a library function nobody defines, also
becomes an import; the error shows only when m-browser instantiates QEMU.

### 2. Build both flavors, check the import, commit

```sh
# from the qemu-wasi root
make && make THREADS=1          # qemu-build/ and qemu-build-threads/
wasm-tools print --skeleton qemu-build-threads/qemu-system-x86_64 \
    | grep -E 'import "env" "host_x"'                # ends in (type N)
# from qemu-src/: fold the change into the patch of its topic
git log --oneline v9.1.0..HEAD          # the hashes change at each rebase
git add <files> && git commit --fixup=<hash of the patch>
GIT_SEQUENCE_EDITOR=true git rebase -i --autosquash v9.1.0
# from the qemu-wasi root
make export-patches && git add patches/ && git commit
```

`grep -E '\(type \(;N;\)'` on the `wasm-tools` output shows the wasm
signature, which step 3 must match. The topic's patch is 0006 for the
network, 0007 for the reactor exports, and so on; choosing it, or adding
a patch, is explained in the qemu-wasi repository's
docs/10-changing-qemu.md. `display/` is plain qemu-wasi code, committed
directly. Do not `make install` before step 3 is done (step 8).

### 3. Register it in m-browser

`linker.func_wrap("env", "host_x", |caller: Caller<'_, QemuHostState>,
...| ...)` registers the closure (a whole one is in the example below);
the optional `Caller` gives the calling Store's data (`caller.data()`,
`data_mut()`). Put it with its area (`add_jit_to_linker`,
`add_gfx_to_linker`, `add_net_to_linker`) or in an `add_<area>_to_linker`
of a new module (`mod` line in `src/main.rs`), called from
`run_wasm_module` in `src/wasm.rs`, after the others:

```rust
    crate::qemu_net::add_net_to_linker(&mut linker)?;
    crate::qemu_log::add_log_to_linker(&mut linker)?;   // new
```

There it serves both builds and precedes the multi-threaded branch's
`instantiate_pre`, which resolves every import once for all threads.
Imports of the multi-threaded build only may go into
`add_threads_to_linker`. State kept between calls is made in the `add_*`
function and moved into the closure as an `Arc` clone (the `VirtualNet`
of `add_net_to_linker`). A Linker refuses to define a name twice.

### 4. Touch QEMU's memory only through with_guest and with_guest_mut

QEMU's pointers are offsets into its linear memory.
`crate::qemu_jit::with_guest(&caller, ptr, len, |bytes| ...)` reads and
`with_guest_mut(&mut caller, ptr, len, |bytes| ...)` writes; both return
`None` out of bounds and handle both builds' memories
([07-jit-host.md](07-jit-host.md)).

- Write `ptr as u32 as usize` for an `i32` (or declare it `u32`): a
  negative `i32` cast to `usize` sign-extends.
- Strings carry no NUL; `String::from_utf8_lossy(bytes)` never panics.
- Numbers are little-endian (`u32::from_le_bytes`), in wasm32's C layout:
  pointers, `int` and `long` 4 bytes, `int64_t` 8.
- Copy inside the closure and never keep the slice: QEMU reuses its
  buffers, and other threads may write shared memory meanwhile.

### 5. Be thread-safe

In the multi-threaded build, the installed one
([08-threads.md](08-threads.md)), each QEMU thread has its own Store:

- One closure serves all threads, at the same time. wasmtime requires it
  to be `Send + Sync + 'static`, so the compiler rejects state that is
  not thread-safe: share state through an `Arc`, with a `Mutex` or
  atomics for whatever changes.
- `QemuHostState` is per Store, so per thread, and short-lived: vCPU
  threads replace their Stores, the single-threaded build moves to fresh
  ones (`migrate_store`, which resets the per-Store fields). A new field
  is set where both builds make one (`run_wasm_module`,
  `QemuThreads::new_store`); keep there nothing other threads need, and
  no wasmtime handle (`Func`, `Memory`) in shared state: a handle belongs
  to one Store.
- Never hold a lock of your own while calling into wasm (an export, a
  TB): that code may call a host function taking the same lock (Rust's
  `Mutex` is not re-entrant), or wait for a QEMU lock whose holder waits
  for yours. Either way, a deadlock.
- WASI functions hold the shared WASI lock until they return to wasm:
  never call `wasi_ctx()` while holding another lock.
- Return quickly: the caller may hold QEMU's big lock (BQL), which every
  vCPU needs now and then. Wait where QEMU expects it (`host_net_wait`)
  or on a thread of your own (the network thread).

### 6. Match the wasm types

| C type in the declaration | Wasm type | Closure parameter or result |
|---|---|---|
| `int32_t`, `int`, `long`, `bool`, `char`, enums | `i32` | `i32` |
| `uint32_t`, `size_t`, pointers | `i32` | `u32` (or `i32`) |
| `int64_t`, `uint64_t` | `i64` | `i64`, `u64` |
| `float`, `double` | `f32`, `f64` | `f32`, `f64` |
| `void` result | none | `()` |
| structs, arrays, strings | avoid by value | a pointer (and a length) |

Wasm has no unsigned types: `u32` and `i32` are the same bits, so Rust's
`u32::MAX` is C's `0xffffffffu` (`JIT_LOAD_FAILED`, `WASI_JIT_FAILED`).
Booleans travel as `i32`: test `!= 0`, return `i32::from(flag)`. `long`
and `size_t` are 32 bits in wasm32: say `int64_t` for 64 bits. clang
passes most structs through a hidden pointer, so the import would not
have the type the C suggests.

### 7. Report errors with return codes; never panic

Values from QEMU are untrusted: use `get`, `checked_add` and
`from_utf8_lossy`, not indexing or `unwrap()`. Return an error code (-1,
a length of 0, handle 0) and log once with the area's prefix
(`[qemu-net]`). The alternatives are worse:

- A panic is not stopped at the boundary: wasmtime unwinds QEMU's frames
  and raises it again in the Rust code that called QEMU. On QEMU's main
  thread, QEMU's run ends uncleanly (the GUI's QEMU thread dies; the
  headless runner exits); on another QEMU thread only that OS thread
  ends, and QEMU most likely hangs.
- Returning `Err` (closures may return `wasmtime::Result<T>`) is a trap.
  On the main thread `step failed` ends QEMU; on any other thread of the
  multi-threaded build, `thread_failed` ends the whole m-browser process
  (wasi-threads' rule). Keep it for "QEMU cannot go on".

### 8. Ship both sides together

If QEMU imports a function m-browser lacks, or with another type,
instantiation fails with an error naming it, such as ``unknown import:
`env::host_x` has not been defined``, and QEMU does not start. Extra
definitions in m-browser are harmless; the legacy JIT functions stay for
that reason. So build m-browser first, which then runs old and new QEMU
builds alike, then install QEMU (`make THREADS=1 install` in qemu-wasi).
To change an existing import's signature, add a new name and remove the
old one once no build uses it: any order would break one side.

### 9. Update the documentation

The area's table in [reference/host-imports.md](reference/host-imports.md)
(and its overview for a new area) and the area's document here; in
qemu-wasi, docs/reference/host-interface.md, the host function table in
README.md, and the comment above the C declaration.

### 10. Test

1. Unit tests for logic kept in plain Rust that needs no Store (as
   `parse_hostfwd`, or the network's `Stack`): `cargo test --release
   --offline <name>` ([14-testing-and-debugging.md](14-testing-and-debugging.md)).
2. A boot of the new build with the headless runner, before installing
   it ([11-headless-and-automation.md](11-headless-and-automation.md)),
   never while the GUI runs QEMU on the same image (the headless runner
   takes no lock):

   ```sh
   # from the m-browser directory
   cargo build --release --offline
   QEMU_QUIT_AFTER=300 ./target/release/m-browser --qemu-headless \
       ../qemu-wasi/qemu-build-threads/qemu-system-x86_64 \
       -L /bios -drive file=/data/m.qcow2,format=qcow2 -m 1024
   ```

3. After `make THREADS=1 install`, the GUI smoke test; near the JIT, also
   qemu-wasi's instruction test (`make test`, `make THREADS=1 test`, with
   `QEMU_JIT_HOT=0` and `4000000000` too).

### Example: host_log (sketch, not tested)

It prints a message from QEMU on m-browser's stderr. In qemu-wasi's
`qemu-src/system/main.c` (patch 0007), in its `#ifdef WASI_PLATFORM`
part:

```c
/*
 * Print a message on the host's stderr (m-browser: src/qemu_log.rs).
 * Returns 0, or -1 if the host could not read it.
 */
extern int32_t host_log(const char *msg, int32_t len);

/* ... in wasi_qemu_init(), after qemu_init(argc, argv): */
    {
        static const char msg[] = "machine created";

        host_log(msg, sizeof(msg) - 1);
    }
```

In m-browser, a new `src/qemu_log.rs`, with `mod qemu_log;` in
`src/main.rs` and the call in `run_wasm_module` shown in step 3:

```rust
//! env.host_log: messages from QEMU, printed by the host.
use crate::qemu_jit::{with_guest, QemuHostState};
use wasmtime::{Caller, Linker};

pub fn add_log_to_linker(linker: &mut Linker<QemuHostState>) -> anyhow::Result<()> {
    linker.func_wrap(
        "env",
        "host_log",
        |caller: Caller<'_, QemuHostState>, ptr: u32, len: u32| -> i32 {
            // Copy the message out: the slice is only valid in here.
            let msg = with_guest(&caller, ptr as usize, len as usize, |bytes| {
                String::from_utf8_lossy(bytes).into_owned()
            });
            let Some(msg) = msg else { return -1 }; // out of bounds
            eprintln!("[qemu-log] {msg}");
            0
        },
    )?;
    Ok(())
}
```

It keeps no state, so any thread may call it. Either QEMU build then
prints `[qemu-log] machine created` before the runner's `Running` line;
reference/host-imports.md gets a row for `env.host_log`.

## Part 2: an environment-variable knob

| | Read by QEMU (C) | Read by m-browser (Rust) |
|---|---|---|
| Choose it when | QEMU's behaviour changes (JIT tiers, display zoom) | the host's does (sockets, threads, caches, the GUI) |
| Read with | `getenv("QEMU_X")` | `std::env::var("QEMU_X")`; `var_os(..).is_some()` for a flag |
| How it gets there | only if the name starts with `QEMU_` or `WASI_`: `run_wasm_module` copies those, and nothing else, into QEMU's WASI environment | directly |
| Read it once, in | a `static`: `wasi_debug_level()` in `include/qemu/wasi-debug.h`, `QEMU_JIT_HOT` in `tcg/wasm32.c` | a `OnceLock` (`migrate_after_instances()` in `src/wasm.rs`) or a constructor (`Stack::new` reads `QEMU_NET_DEBUG`) |
| Document it in | the qemu-wasi repository's docs/reference/environment-variables.md (see its "Adding a variable") | [reference/environment-variables.md](reference/environment-variables.md) and the area's document |

Names: `QEMU_<AREA>_<SETTING>` (`QEMU_JIT_THREADS`, `QEMU_NET_DEBUG`),
`WASI_GFX_*` for QEMU's display backend, `_DEBUG` and `_STATS` for
diagnostics, and the unit in the name when it is not obvious
(`QEMU_TB_SLICE_MS`). m-browser's own `QEMU_*` variables reach QEMU too,
harmlessly as long as QEMU reads no variable of the same name.

- Read once: a lookup scans the whole environment, and functions such as
  `jit_load` or `host_net_recv` run thousands of times a second.
- A presence flag is on even with `=0`. If people may write `=0` to mean
  off, test the value, as `QEMU_JIT_ASYNC=0` does.
- Bad numbers fall back silently: to the default with m-browser's
  `.ok().and_then(|v| v.parse().ok()).unwrap_or(..)` pattern, but to 0
  with C's `atoi`, which may be a valid and surprising setting.
- In the multi-threaded build several threads may run a C `static`'s
  first read at once: harmless only if they all compute the same value.
- The environment is fixed when m-browser starts: restart to change it.

## Part 3: worked example, UDP port forwarding

*Port forwarding* lets programs on the host or its network reach a
server in the guest. QEMU's rule is
`hostfwd=udp:[hostaddr]:hostport-[guestaddr]:guestport`, given to the GUI
in `QEMU_NIC`. This design is not implemented: the guest's own UDP and
DNS work, inbound UDP does not. Background:
[10-networking.md](10-networking.md).

### What exists today

- `host_net_hostfwd` calls `VirtualNet::add_forward` on QEMU's main
  thread during `wasi_qemu_init`; `tcp::parse_hostfwd` refuses `udp`
  ("forwarding UDP is not supported yet"), so QEMU stops with "could not
  set up host forwarding rule".
- TCP: `add_forward` binds a `std::net::TcpListener` there (a port in use
  fails QEMU's start-up) and queues it in `Shared::new_forwards`;
  `Stack::run`, on the network thread, passes it to
  `TcpNat::add_forward`. Event tokens found in `Stack::sockets` go to
  `socket_readable`, others to `TcpNat::host_event`. Forwarded
  connections come from 10.0.2.2 and a port of `FORWARD_PORTS`
  (49152-65535), from `free_forward_port`.
- The guest's UDP: after the DHCP check (destination port 67),
  `Stack::udp` maps the destination with `host_address()` (10.0.2.2 is the
  host's 127.0.0.1); `udp_from_guest` sends through a connected socket
  (`open_udp_socket`) per `Flow::Udp` (`udp_flows`), a `HostSocket` in
  `Stack::sockets` that `close_idle_sockets` closes after its
  `idle_timeout`. `send_udp` builds the guest's frames (`send_ipv4_to`,
  `checksum`, `transport_checksum`).

### Design

```text
peer 192.168.1.7:40000             m-browser                       guest
  --- to host:5555 --->  listener: Flow::UdpForward, in Stack::sockets
                         session (listener, peer) = 10.0.2.2:49152
                         send_udp(10.0.2.2:49152 -> 10.0.2.15:5000) --->
  <-- listener.send_to   Stack::udp: (10.0.2.15:5000, 10.0.2.2:49152)
                         is a session, found before host_address() <---
```

- **One listening socket per rule**, bound in `add_forward` as for TCP
  (errors still stop QEMU's start-up), kept as a `HostSocket` with the
  new `Flow::UdpForward { guest }`: `run` then passes its events to
  `socket_readable`.
- **A session per peer.** The guest sees every peer as 10.0.2.2, so only
  the source port can tell peers apart, and two peers may use the same
  one: each new peer gets a port from `FORWARD_PORTS`, as TCP forwards
  do. A session ends after `IDLE_TIMEOUT` (60 s) of silence.
- **Replies leave through the listener** (`send_to(peer)`): clients
  expect the answer from the address they sent to, and many drop others.
- **Replies are matched first**: a datagram to 10.0.2.2:p of a session
  must not reach `host_address()`, which would send it to host port p.

### What to change

| File | Item | Change |
|---|---|---|
| `src/qemu_net/tcp.rs` | `parse_hostfwd` | Accept `udp`; also return the protocol (a new `pub(super) enum Protocol { Tcp, Udp }`); `FORMAT` reads `[tcp\|udp]` |
| | `FORWARD_PORTS`; test `hostfwd_rules` | Make it `pub(super)`, for the sessions; the test expects the protocol, and `udp::5353-:53` now parses |
| `src/qemu_net.rs` | `Shared::new_forwards` | `Vec<NewForward>`, a new enum: `Tcp(mio::net::TcpListener, SocketAddrV4)` or `Udp(UdpSocket, SocketAddrV4)` |
| | `VirtualNet::add_forward` | For UDP: bind a `std::net::UdpSocket`, `set_nonblocking(true)`, `local_addr()`, wrap it with mio's `UdpSocket::from_std`, queue `NewForward::Udp`. Same return type, so callers and tests stay |
| | `Flow` | New variant `UdpForward { guest: SocketAddrV4 }` |
| | `Stack`, `Stack::new` | New fields `udp_sessions`, `udp_session_flows`, `next_session_port`; new struct `UdpSession` |
| | `Stack::run` | Keep `let forwards = take(...)` (the lock is then not held in the loop); match: `Tcp` to `self.tcp.add_forward` as now, `Udp` to `self.add_socket(socket, Flow::UdpForward { guest }, Duration::MAX)` |
| | `Stack::socket_readable` | Arm `Flow::UdpForward { guest } => self.forward_to_guest(token, guest, from, &data)` |
| | new `forward_to_guest`, `free_session_port`, `forward_to_peer` | Sessions, both directions |
| | `Stack::udp` | Session lookup right after the DHCP check |
| | `Stack::close_idle_sockets` | Skip listeners; expire sessions |
| | module comment; `add_net_to_linker` | Mention UDP; the log line may print the protocol |

### Sketches (sketch, not tested)

The new `Stack` fields: `udp_sessions: HashMap<(Token, SocketAddr),
UdpSession>` (by listener and peer), `udp_session_flows:
HashMap<(SocketAddrV4, SocketAddrV4), (Token, SocketAddr)>` (by guest and
10.0.2.2 address, for replies) and `next_session_port: u16`, starting at
`*tcp::FORWARD_PORTS.start()`. `free_session_port(guest)` works like
`TcpNat::free_forward_port`: it walks `FORWARD_PORTS` from
`next_session_port`, wrapping around, and returns the first port whose
`(guest, 10.0.2.2:port)` is in neither `udp_session_flows` nor
`udp_flows` (a flow the guest opened to that host port itself).

```rust
/// A peer of a UDP port forward; the guest sees it as `remote`.
struct UdpSession { guest: SocketAddrV4, remote: SocketAddrV4, last_used: Instant }

fn forward_to_guest(&mut self, listener: Token, guest: SocketAddrV4, peer: SocketAddr, data: &[u8]) {
    let remote = match self.udp_sessions.get_mut(&(listener, peer)) {
        Some(session) => {
            session.last_used = Instant::now();
            session.remote
        }
        None => {
            let Some(port) = self.free_session_port(guest) else { return };
            let remote = SocketAddrV4::new(GATEWAY_IP, port);
            let session = UdpSession { guest, remote, last_used: Instant::now() };
            self.udp_sessions.insert((listener, peer), session);
            self.udp_session_flows.insert((guest, remote), (listener, peer));
            remote
        }
    };
    self.log(format_args!("UDP {peer} -> {guest} as {remote}: {} bytes", data.len()));
    self.send_udp(remote, guest, data);
}

// In Stack::udp, right after `if remote.port() == 67 { ... }`:
//     if let Some(&(listener, peer)) = self.udp_session_flows.get(&(guest, remote)) {
//         return self.forward_to_peer(listener, peer, data);
//     }

fn forward_to_peer(&mut self, listener: Token, peer: SocketAddr, data: &[u8]) {
    if let Some(session) = self.udp_sessions.get_mut(&(listener, peer)) {
        session.last_used = Instant::now();
    }
    let Some(s) = self.sockets.get(&listener) else { return };
    if let Err(e) = s.socket.send_to(data, peer) {
        self.log(format_args!("UDP to {peer}: {e}"));
    }
}
```

In `close_idle_sockets`, add `!matches!(s.flow, Flow::UdpForward { .. })
&&` to the idle filter and an arm `Flow::UdpForward { .. } => None` to
the match (never reached, but a match must be complete). At its end,
`self.udp_sessions.retain(...)` drops the sessions idle for longer than
`IDLE_TIMEOUT`, removing each one's `(guest, remote)` from
`udp_session_flows` as well: the two maps must always agree.

### Pitfalls

**Pitfall:** listeners must never expire. QEMU sets forwards up once, at
start-up: a listener closed after 60 quiet seconds would never return.

**Pitfall:** keep the port-67 check first in `Stack::udp`: DHCP must be
answered whatever else is configured.

**Pitfall:** with a listener in `Stack::sockets`, `busy` in `Stack::run`
stays true, so the network thread wakes every 5 s even when idle.
Harmless; to avoid it, leave listeners out of `busy` but count sessions,
or idle sessions would only expire at the next event.

**Pitfall:** `send_udp` drops datagrams until the guest has sent a frame,
as it needs the guest's MAC address; tests must announce the guest first.

**Pitfall:** the parser test `hostfwd_rules` asserts today that UDP rules
fail, and smoltcp is built without UDP sockets (see Tests).

**Note:** the guest must answer from the forwarded port, as `nc` and most
servers do; an answer from another port goes to `host_address()`.

### QEMU's side

No code change: `net_init_wasi_user` in `qemu-src/net/wasi-user.c.inc`
passes each `hostfwd=` value to `host_net_hostfwd` verbatim. Only its
comment, which gives the format as `[tcp]:...`, should say `[tcp|udp]`:
a fixup of patch 0006 (Part 1, step 2). No reinstall is needed for that.

### Tests

The unit tests in `src/qemu_net.rs` drive a real `VirtualNet` with
smoltcp as the guest's stack (`TestGuest`), but smoltcp is built with
`socket-tcp` only (`Cargo.toml`), so UDP frames are built by hand (or
enable `socket-udp`, under `[dev-dependencies]` to keep it out of the
normal build). In `hostfwd_rules`, the `Ok` results gain the protocol and
the `udp::5353-:53` line becomes an `Ok` check. The new test needs two
helpers in the tests module: `next_frame(&net, |frame| ...)` polls
`net.to_guest()` every millisecond until a frame passes the test
(skipping others) and panics after 10 s; `guest_udp_frame(src, dst,
data)` builds what the guest would send: an Ethernet header (to
`GATEWAY_MAC`, from `TEST_GUEST_MAC`, `ETH_IPV4`), a 20-byte IPv4 header
as the test helper `fragment()` builds one, but unfragmented and with
protocol `IP_UDP`, and an 8-byte UDP header whose checksum 0 means
"none" (the stack checks no checksums).

```rust
#[test]
fn udp_port_forward_into_the_guest() {
    within(Duration::from_secs(30), || {
        let g = TestGuest::new();
        let (host, guest) = g.net.add_forward("udp:127.0.0.1:0-:5000").unwrap();
        g.announce(); // once its ARP reply is out, the network knows the guest
        next_frame(&g.net, |f| f[12..14] == ETH_ARP.to_be_bytes());

        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client.send_to(b"hello guest", host).unwrap();
        let f = next_frame(&g.net, |f| {
            f.len() > 42 && f[12..14] == ETH_IPV4.to_be_bytes() && f[23] == IP_UDP
        });
        let field = |at: usize| u16::from_be_bytes([f[at], f[at + 1]]);
        let remote = SocketAddrV4::new(ipv4(&f[26..30]), field(34));
        assert_eq!(*remote.ip(), GATEWAY_IP);
        assert_eq!(SocketAddrV4::new(ipv4(&f[30..34]), field(36)), guest);
        assert_eq!(&f[42..34 + field(38) as usize], b"hello guest");

        g.net.from_guest(&guest_udp_frame(guest, remote, b"hello host"));
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 64];
        let (n, from) = client.recv_from(&mut buf).unwrap();
        assert_eq!((&buf[..n], from), (&b"hello host"[..], host));
    });
}
```

Worth adding: a second client gets another 10.0.2.2 port; and, on a
`test_stack()`, a session whose `last_used` lies two minutes back is gone
after `close_idle_sockets` while its listener stays. Run them with
`cargo test --release --offline udp`, from the m-browser directory.

### Checking by hand

1. Build and start with a rule and the network log:

   ```sh
   # from the m-browser directory
   cargo build --release --offline
   QEMU_NIC=user,hostfwd=udp:127.0.0.1:5555-:5000 QEMU_NET_DEBUG=1 \
       ./target/release/m-browser 2>&1 | tee /tmp/m-browser.log
   ```

2. Open QEMU's page; the log shows `[qemu-net] forwarding
   127.0.0.1:5555 to the guest's 10.0.2.15:5000`.
3. In a guest terminal: `nc -u -l -p 5000`. busybox's `nc` options depend
   on its build: check `nc --help` (`apk add netcat-openbsd` gives an
   `nc` whose syntax is `nc -u -l 5000`).
4. On the host: `echo hi | nc -u 127.0.0.1 5555`, and `hi` appears in the
   guest. Run the host's `nc -u 127.0.0.1 5555` interactively instead,
   and lines typed in the guest come back too.
5. With the host port taken by another program, QEMU must refuse to start
   (`cannot listen on 127.0.0.1:5555`).

### Documentation to update

- m-browser: [10-networking.md](10-networking.md) (port forwarding);
  [reference/host-imports.md](reference/host-imports.md)
  (`host_net_hostfwd` says "a host TCP port");
  [reference/environment-variables.md](reference/environment-variables.md)
  (`QEMU_NIC` says "TCP only"); the test table and count in
  [14-testing-and-debugging.md](14-testing-and-debugging.md); the module
  comment of `src/qemu_net.rs`. (m-browser's README.md does not mention
  forwarding.)
- qemu-wasi: README.md ("no UDP port forwarding" under Limitations, the
  `hostfwd=[tcp]:...` form at `QEMU_NIC`), docs/reference/host-interface.md,
  docs/reference/glossary.md (hostfwd), the comment in
  `net/wasi-user.c.inc`.

## Part 4: pointers for other common features

- **A new input event type.** Events are 12-byte `(type, code, value)`
  records, which `host_gfx_poll_events` copies without looking: it needs
  no change. Take the next free type (6); produce it in
  `App::window_event()` (`src/app.rs`) and, if scripts need it, in
  `src/headless.rs`; handle it in the `switch` of
  `wasi_gfx_display_poll_events()` in qemu-wasi's
  `display/wasi_gfx_display.c`; update the type lists in the comments on
  `InputEvent` and above `host_gfx_poll_events`. QEMU drops unknown types
  (`default:`), so either side may ship first. Decide whether the type
  counts against `KEYS_PER_POLL`, as keys and text do. See
  [09-display-and-input.md](09-display-and-input.md).
- **A key that does nothing.** A line in `keycode_to_evdev()`
  (`src/qemu_keymap.rs`), following "To add a key" in
  [09-display-and-input.md](09-display-and-input.md).
- **QEMU's default arguments.** The `args` vector in the core-module
  branch of `App::window_event()` (`src/app.rs`); the headless runner
  takes its own command line, and `run_wasm_module` adds `-smp` for the
  multi-threaded build. See "Change QEMU's arguments" in
  [05-browser-ui.md](05-browser-ui.md).
- **A menu item.** In `handle_redraw()` (`src/app.rs`): "Add a menu item"
  in [05-browser-ui.md](05-browser-ui.md).
- **A JIT knob in m-browser** (`src/qemu_jit.rs`). Read it once, as
  `BackgroundCompiler::start` reads `QEMU_JIT_THREADS` or as
  `dump_tb_module` uses a `OnceLock`, never per `jit_load` call, a hot
  path; list it in the tuning table of [07-jit-host.md](07-jit-host.md)
  and in the reference (Part 2). Knobs of QEMU's JIT tiers live in
  `tcg/wasm32.c`.
- **IPv6** would be a large project, because everything assumes IPv4.
  The guest needs an address plan (QEMU's slirp uses fec0::/64, host ::2,
  DNS ::3) handed out by router advertisements (SLAAC) or DHCPv6;
  neighbour discovery (ICMPv6) replaces ARP (`Stack::arp`);
  `Stack::guest_frame` drops all but ARP and IPv4; `Packet`, `ipv4()`,
  `send_ipv4_to` and its fragmenting, `transport_checksum`'s
  pseudo-header, `Flow`, `host_address()`, `tcp::Flow` and
  `parse_hostfwd` all use IPv4 types; smoltcp is built with `proto-ipv4`
  only; and QEMU's `wasi_user_unsupported_option()` rejects `ipv6=on` and
  the `ipv6-*` options.
