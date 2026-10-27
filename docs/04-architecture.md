# Architecture: a map of m-browser

**What this covers.** Which source file does what, and which chapter
explains it in depth; the two run modes (GUI and headless) and the two QEMU
flavors (single-threaded and multi-threaded); the threads that exist while
QEMU runs, and the wasmtime Stores they use; how data flows (startup, one
main-loop step, a translation block, a frame, a key press, a network
packet, shutdown); who owns which state, why the main design decisions were
made, and where to start for common tasks.

This chapter is a map, not a manual: every part has its own chapter, linked
from the tables below. QEMU's side of the interface (how QEMU itself was
ported to WebAssembly) is described in the qemu-wasi repository, in
docs/04-architecture.md and docs/reference/host-interface.md. If words such as
*Store*, *Instance*, *Linker* or *host import* are new to you, read
[02-concepts.md](02-concepts.md) first.

## Summary

m-browser is one native program. Its main thread runs a winit event loop and
draws the browser itself (address bar, tabs, Markdown pages) with egui on
wgpu. Opening a `.wasm` file runs it on a thread of its own, showing its
output in a second window, the *child window*. Most of the code exists for one
such file: `qemu-system-x86_64.wasm`, the QEMU emulator compiled to a
WebAssembly core module by the qemu-wasi repository, which boots an Alpine
Linux desktop from `m.qcow2`. wasmtime runs QEMU, and m-browser supplies, as
*host imports*, everything QEMU cannot do inside WebAssembly: turning the
guest code that QEMU translated into native code (`src/qemu_jit.rs`), the
screen, keyboard and mouse (`src/qemu_gfx.rs`), a virtual network
(`src/qemu_net.rs`), threads (`src/qemu_threads.rs`), and files and clocks
(wasmtime-wasi). QEMU is built as a *reactor*: instead of running its main
loop forever, it exports `wasi_qemu_step`, which m-browser calls in a loop.
Between two calls the host has control again, which it uses to stop QEMU
cleanly and to move QEMU into a fresh Store when compiled code piles up.

## Components

```text
 +--------------------------------------------------------------------+
 | UI thread (main)                           src/main.rs, src/app.rs |
 |   winit event loop -> App (window events, redraws)                 |
 |   egui + wgpu: address bar, tabs, Markdown pages                   |
 |   child window: QEMU's screen; keys and mouse -> GfxEventQueue     |
 +--------------------------------------------------------------------+
        |  std::thread::spawn, handing over: the child window's Surface,
        |  the GfxEventQueue and a stop flag (Arc<AtomicBool>)
        v
 +--------------------------------------------------------------------+
 | QEMU thread                                            src/wasm.rs |
 |   run_wasm_module -> run_qemu_reactor                              |
 |   Store<QemuHostState> holding QEMU's Instance                     |
 |   loop { wasi_qemu_step(); maybe migrate_store() }                 |
 +--------------------------------------------------------------------+
        |  QEMU's code calls host imports (functions in the Linker)
        v
 +------------+ +------------+ +------------+ +------------+ +------------+
 | qemu_jit   | | qemu_gfx   | | qemu_net   | |qemu_threads| | wasmtime-  |
 | env.wasi_  | | env.host_  | | env.host_  | | wasi.      | | wasi       |
 |   jit_*    | |   gfx_*    | |   net_*    | |  thread-   | | wasi_snap- |
 |            | |            | |            | |  spawn,    | |  shot_     |
 |            | |            | |            | | env.host_  | |  preview1  |
 |            | |            | |            | |  vcpu_*    | |            |
 +-----+------+ +-----+------+ +-----+------+ +-----+------+ +-----+------+
       |              |              |              |              |
 helper threads       |              |              |              |
   qemu-jit-N         |          qemu-net       qemu-<tid>         |
   (Cranelift)        |          (mio loop)     (RCU, vCPUs)       |
       |              |              |              |              |
 host resources       |              |              |              |
   ~/.cache/      child window   host sockets   (same as the   m.qcow2,
   m-browser      (softbuffer)   UDP, TCP,      other columns) pc-bios/,
   (QEMU .cwasm)  or PNG files   ICMP                          stdout
```

With `QEMU_PROFILE=1` one more helper thread, `qemu-profile`, ticks the
sampling profiler. wasi-gfx components (`breakout.wasm`, `triangle.wasm`) take
a separate, much smaller path through `Wasm` in `src/wasm.rs` and
`src/winit_wasi.rs`; it is described in
[05-browser-ui.md](05-browser-ui.md) and [06-qemu-runner.md](06-qemu-runner.md).

## Source files

| File | Responsibility | Key types and functions | Chapter |
|---|---|---|---|
| `src/main.rs` | Entry point: headless mode or the winit event loop | `main`, `run` | [05](05-browser-ui.md), [11](11-headless-and-automation.md) |
| `src/app.rs` | The browser: windows, tabs, pages, starting and stopping QEMU, routing input | `App`, `AppState`, `Tab`, `QemuRun`, `lock_disk_image`, `stop_qemu`, `shutdown_qemu` | [05](05-browser-ui.md) |
| `src/egui_tools.rs` | egui drawn with wgpu | `EguiRenderer` | [05](05-browser-ui.md) |
| `src/fill.rs` | Painting a window with a solid color through softbuffer | `fill_window` | [05](05-browser-ui.md) |
| `src/winit_wasi.rs` | A winit window as a wasi-gfx surface | `MyWindowWrapper` | [05](05-browser-ui.md), [09](09-display-and-input.md) |
| `src/wasm.rs` | Running wasm: the component runtime and the QEMU runner | `Wasm`, `run_wasm_module`, `load_qemu_module`, `run_qemu_reactor`, `migrate_store` | [06](06-qemu-runner.md) |
| `src/qemu_jit.rs` | Translation blocks to native code; the data of every QEMU Store | `JitCache`, `BackgroundCompiler`, `QemuJitState`, `QemuHostState`, `GuestMem`, `with_guest`, `add_jit_to_linker` | [07](07-jit-host.md) |
| `src/qemu_threads.rs` | wasi-threads for the multi-threaded QEMU | `QemuThreads`, `SharedWasi`, `run_vcpu`, `add_threads_to_linker` | [08](08-threads.md) |
| `src/qemu_gfx.rs` | QEMU's screen and input | `QemuGfxState`, `GfxEventQueue`, `InputEvent`, `add_gfx_to_linker` | [09](09-display-and-input.md) |
| `src/qemu_keymap.rs` | winit key codes to Linux evdev codes | `keycode_to_evdev` | [09](09-display-and-input.md) |
| `src/qemu_net.rs` | The guest's network: ARP, DHCP, DNS, UDP, ICMP, port forwarding | `VirtualNet`, `Shared`, `Stack::run`, `add_net_to_linker` | [10](10-networking.md) |
| `src/qemu_net/tcp.rs` | TCP: a NAT built on smoltcp | | [10](10-networking.md) |
| `src/qemu_profile.rs` | Sampling profiler of QEMU | `configure`, `start`, `attach`, `time_host`, `HostWork` | [12](12-profiling.md) |
| `src/headless.rs` | `--qemu-headless`: QEMU without a window | `run` | [11](11-headless-and-automation.md) |

The full list of imports and exports is in
[reference/host-imports.md](reference/host-imports.md); every environment
variable is in
[reference/environment-variables.md](reference/environment-variables.md).

## Two run modes

`main` looks at its first argument:

- **GUI** (`m-browser`): the winit event loop and `App`. QEMU starts when a
  page's URL ends in its `.wasm` file, runs on its own thread, draws into the
  child window and receives input from it. The QEMU command line is fixed in
  `src/app.rs`, which also locks the disk image.
- **Headless** (`m-browser --qemu-headless ...`): `headless::run` and no
  window. The remaining arguments choose the module and QEMU's options; the
  screen is written to PNG files, and keys come from `QEMU_TYPE` or
  `QEMU_TYPE_FIFO`. It is meant for automation and tests; see
  [11-headless-and-automation.md](11-headless-and-automation.md).

Both modes end up in the same `run_wasm_module`, so everything below the QEMU
thread in the diagram is shared. Only the source of input, the display target
(`surface` is `None` when headless) and who sets the stop flag differ.

## Two QEMU flavors

qemu-wasi builds QEMU in two flavors, and `run_wasm_module` tells them apart by
one import: a module that imports `env.memory` (a shared memory) is the
multi-threaded build.

- **Single-threaded**: one Store on the QEMU thread. QEMU runs its vCPUs in
  turn inside `wasi_qemu_step`. This Store owns its WASI context
  (`WasiSlot::Own`) and is replaced by `migrate_store` when translation-block
  instances pile up.
- **Multi-threaded** (MTTCG, one thread per vCPU): QEMU starts its threads
  through the `wasi.thread-spawn` import. Every thread gets a Store and an
  Instance of its own, all importing the same `SharedMemory`. The WASI context
  is shared (`SharedWasi`, used by one thread at a time) and each vCPU thread
  replaces its own Store when needed; the main-loop Store never runs guest
  code and is never replaced. The vCPU count is QEMU's `-smp`, which
  m-browser appends when the arguments have none: `QEMU_SMP`, else 4 (2 on
  hosts with fewer than 8 CPUs).

```text
 single-threaded build              multi-threaded build
 (no env.memory import)             (imports env.memory)

 QEMU thread                        QEMU thread    qemu-1       qemu-2  ...
 +--------------------+             +-----------+  +----------+ +----------+
 | Store              |             | Store     |  | Store    | | Store    |
 |  QEMU Instance     |             |  QEMU     |  |  QEMU    | |  QEMU    |
 |  its own memory    |             |  Instance |  |  Instance| |  Instance|
 |  TB instances      |             |  (main    |  |  TB      | |  TB      |
 |  WasiSlot::Own     |             |   loop)   |  |  inst.   | |  inst.   |
 +--------------------+             +-----+-----+  +----+-----+ +----+-----+
  replaced by migrate_store               |             |            |
  when TB instances pile up               +-------------+------------+
                                          |                          |
                                     SharedMemory               SharedWasi
                                     (QEMU's memory)            (one WASI ctx)

 Shared by every Store in both builds: JitCache (compiled TB modules),
 Arc<Mutex<QemuGfxState>> (display), the GfxEventQueue, the network.
```

The details, including how a vCPU thread enters QEMU (`host_vcpu_run`,
`wasi_vcpu_enter`), are in [08-threads.md](08-threads.md).

## Data flows

### 1. Startup: from the address bar to the first frame

1. The user opens `file:///.../qemu-system-x86_64.wasm`. `App` recognizes a
   wasm page and sets `spawn_child_window`.
2. In `App::window_event`, the UI thread stops a previous QEMU if one runs
   (`stop_qemu`), creates the child window, wraps it in a
   `wasi_surface_wasmtime::Surface`, creates a `GfxEventQueue` and a stop
   flag, and spawns the QEMU thread. It keeps a `QemuRun` (stop flag and
   thread handle).
3. The QEMU thread waits for the previous QEMU thread to finish, then locks
   `m.qcow2` (`lock_disk_image`) and calls `run_wasm_module` with QEMU's
   arguments and the preopened directories (`m.qcow2` appears to QEMU as
   `/data/m.qcow2`, `pc-bios` as `/bios`).
4. `run_wasm_module` creates the wasmtime `Engine` and loads QEMU's module
   with `load_qemu_module`: the first time, it is compiled with optimizations
   (this takes a while) and cached in `~/.cache/m-browser/`; later runs load
   the cached `.cwasm` file.
5. It detects the flavor, builds the Linker (WASI, JIT, display, network and,
   for the threaded build, threads), the WASI context (arguments, `WASI_*` and
   `QEMU_*` variables, preopens), the shared display state and the
   `JitCache`, and instantiates QEMU.
6. `run_qemu_reactor` calls `_initialize`, then `wasi_qemu_init`: QEMU parses
   its options, opens the disk image, creates its devices and its display,
   which calls `host_gfx_init`.
7. The loop of `wasi_qemu_step` starts; the guest's firmware draws, QEMU
   calls `host_gfx_update_framebuffer`, and the first frame appears in the
   child window (flow 4).

### 2. One main-loop step and Store migration

1. `run_qemu_reactor` checks the stop flag (flow 7) and calls
   `wasi_qemu_step`.
2. QEMU runs one iteration of its main loop: timers, I/O handlers, display
   refresh, network frames, and (single-threaded build) a slice of guest
   execution. A negative result means "keep going"; zero or more is QEMU's
   exit status.
3. With `QEMU_JIT_STATS` set, the JIT's statistics are printed every minute.
4. In the single-threaded build, the host then counts the TB instances in the
   Store. Above `MIGRATE_AFTER_INSTANCES` (100,000, or `QEMU_MIGRATE_AFTER`),
   or when the process's memory mappings (checked every `MAP_CHECK_INTERVAL`
   instances) exceed `MAP_COUNT_SOFT_LIMIT`, it calls `migrate_store`.
5. `migrate_store` copies the non-zero parts of QEMU's memory, drops the old
   Store (which frees every TB instance), instantiates QEMU in a new Store
   with the same host state, copies the memory back and calls
   `wasi_qemu_jit_reset` so QEMU forgets its compiled-TB handles. It takes
   about half a second. If the cached modules alone use too many mappings, the
   module cache is trimmed as well.

Details: [06-qemu-runner.md](06-qemu-runner.md).

### 3. A translation block's life on the host side

1. QEMU translates a block of guest instructions (a *TB*) into a small wasm
   module in its own memory, and interprets that module at first. Once the
   TB is hot (5000 runs, `QEMU_JIT_HOT`), QEMU passes the module to
   `wasi_jit_load_async`.
2. The host looks the module's bytes up in the `JitCache`. On a miss it hands
   the bytes to the `BackgroundCompiler`, and a `qemu-jit-N` thread compiles
   them with Cranelift (at `OptLevel::None`) while QEMU keeps interpreting
   the block.
3. Once the module is compiled, the host instantiates it in the calling
   thread's Store, sharing an existing instance when the same module is
   already instantiated there, without a Linker.
4. The TB's function is stored in a slot of QEMU's function table
   (`__indirect_function_table`), and QEMU calls it from then on like any of
   its own functions.
5. Nothing frees one TB's slot or instance: identical TBs share them, and a
   slot cannot be reused while QEMU might still hold its index (QEMU never
   calls `wasi_jit_drop`). Both live until the Store is replaced, which is
   why Stores are migrated (flow 2).

Details: [07-jit-host.md](07-jit-host.md).

### 4. A frame

1. QEMU's display code calls `host_gfx_update_framebuffer` with the position
   and size of its framebuffer in its memory.
2. The host reads the pixels through `with_guest` and, in the GUI, copies them
   into the child window with softbuffer and presents them. Headless, the
   latest frame is kept and written to a PNG file when asked.
3. When the guest changes resolution, QEMU calls `host_gfx_resize` first.

Details: [09-display-and-input.md](09-display-and-input.md).

### 5. A key press

1. winit delivers a `WindowEvent::KeyboardInput` to `App::window_event` on the
   UI thread.
2. If the child window is shown and egui does not want the key,
   `keycode_to_evdev` turns the key into a Linux evdev code and an
   `InputEvent` is pushed into the `GfxEventQueue`. Mouse moves and buttons
   take the same queue.
3. During its main loop, QEMU calls `host_gfx_poll_events`, which moves the
   queued events into QEMU's memory.
4. QEMU feeds them to its emulated keyboard and tablet, and the guest sees the
   key.

Details: [09-display-and-input.md](09-display-and-input.md).

### 6. A network packet

1. The guest's network card hands an Ethernet frame to QEMU, which calls
   `host_net_send`.
2. The host copies the frame into the shared network state and wakes the
   `qemu-net` thread.
3. The `qemu-net` thread's event loop (`Stack::run`, built on mio) answers
   ARP and DHCP itself, relays DNS, UDP and ICMP to host sockets, and passes
   TCP to the smoltcp-based NAT in `src/qemu_net/tcp.rs`.
4. Replies from host sockets are turned back into Ethernet frames and queued
   for the guest; queuing one ends the main loop's sleep in `host_net_wait`.
   (QEMU's own threads end that sleep with `host_main_wake`.)
5. QEMU calls `host_net_recv` to take the queued frames and gives them to the
   guest's network card.

Details: [10-networking.md](10-networking.md).

### 7. Shutdown

1. Closing the child window, leaving the page or closing the browser calls
   `App::stop_qemu`: input stops being queued and the stop flag is set.
   Closing the browser (`shutdown_qemu`) also waits a while for the QEMU
   thread.
2. Between two steps, `run_qemu_reactor` sees the flag and calls
   `wasi_qemu_quit`: QEMU powers the guest off and closes its disk images.
3. The next `wasi_qemu_step` returns an exit status, `run_wasm_module`
   returns, and the QEMU thread ends, releasing the disk-image lock.
4. If QEMU has not quit within `QUIT_TIMEOUT` (10 s), the host stops calling
   it and returns anyway.

Details: [05-browser-ui.md](05-browser-ui.md) and
[06-qemu-runner.md](06-qemu-runner.md).

## Threads

| Thread | Created by | Runs | Lifetime |
|---|---|---|---|
| main (UI) | the operating system | winit event loop, `App`, egui and wgpu; headless: `headless::run` and QEMU | the process |
| QEMU thread (unnamed) | `App::window_event` (`std::thread::spawn`) | `lock_disk_image`, `run_wasm_module`, `run_qemu_reactor` | from opening the page until QEMU has quit |
| `qemu-jit-N` | `BackgroundCompiler::start`, at QEMU's first background compile | Cranelift compilation of TB modules | until the QEMU run's `JitCache` is dropped (threaded build: until m-browser exits, see [known-issues.md](known-issues.md)) |
| `qemu-<tid>` | `QemuThreads::spawn` (`wasi.thread-spawn`) | one of QEMU's threads in its own Store: usually `qemu-1` is RCU's `call_rcu`, then one per vCPU | meant to end with QEMU; from reading the code they stay blocked until m-browser exits ([known-issues.md](known-issues.md)) |
| `qemu-net` | `VirtualNet::start`, when `run_wasm_module` builds the Linker | `Stack::run`: frames, host sockets | until the QEMU run's network is dropped (threaded build: until m-browser exits) |
| `qemu-profile` | `qemu_profile::start` (`QEMU_PROFILE=1`) | the profiler's sampling tick | until `qemu_profile::stop` |
| component threads (unnamed) | `App::window_event` | a wasi-gfx component (`Wasm`), and a watcher for the close signal | while the component runs |

**Pitfall:** the QEMU thread is not named, so debuggers and `top -H` show it
under the program's name.

## Who owns what state

- **Per Store**: `QemuHostState` (`src/qemu_jit.rs`) is the data of every
  QEMU Store: the WASI context or a handle to the shared one (`WasiSlot`),
  the TB instances of this Store (`QemuJitState`), a handle to the display
  state, QEMU's memory and function table (`guest_memory`, `guest_table`),
  and the Store's resource limits. Migrating a Store moves this data into the
  new Store.
- **Shared between threads** (behind `Arc`): `JitCache` (compiled modules and
  statistics, `Arc<Mutex<...>>`), `QemuGfxState` (display,
  `Arc<Mutex<...>>`), the `GfxEventQueue` (shared with the UI thread),
  `SharedWasi` (with its own lock), `QemuThreads` and the `SharedMemory`
  (threaded build), the stop flag (`Arc<AtomicBool>`).
- **Per QEMU run, held by the Linker's closures**: the network
  (`VirtualNet`, one per Linker, so one per QEMU start).
- **Global**: the profiler, and settings read once from environment
  variables (for example `migrate_after_instances`).

**Note:** anything a host import needs must be reachable from
`QemuHostState` or be global, because an import only receives its Store.
[13-adding-features.md](13-adding-features.md) explains how to add state
safely.

## Key design decisions

- **QEMU as a reactor.** wasmtime frees a module instance only when its Store
  is dropped, and each TB module adds instances and memory mappings; Linux
  limits a process to `vm.max_map_count` mappings (65,530 by default). A
  reactor returns to the host after each main-loop iteration, when no wasm
  frame is live, so the host can move QEMU into a fresh Store (QEMU's state
  is all in its memory) and stop it cleanly.
- **Two optimization levels.** Hundreds of thousands of tiny TB modules are
  compiled during a session, so the engine uses `OptLevel::None`, where
  compile time is lowest. QEMU's own module runs all session long, so it is
  compiled once with `OptLevel::Speed` by a second engine and cached on disk.
- **Shared TB instances.** Loading a module already instantiated in the Store
  reuses that instance rather than creating another, which keeps instance
  counts, memory and migrations down.
- **Background compilation.** Compiling on `qemu-jit-N` threads keeps QEMU
  running guest code while TBs compile instead of stalling on every new
  block, and spreads compilation over the host's cores.
- **The network on its own thread, with mio.** Host sockets must be serviced
  whenever data arrives, not only when QEMU happens to call the host; one
  mio event loop watches every socket without a thread per connection, and
  keeps socket work out of QEMU's main loop.
- **softbuffer in a child window.** QEMU's framebuffer is plain memory on the
  CPU side; softbuffer copies it into a window without a GPU pipeline. A
  separate window keeps QEMU's screen and input apart from egui's wgpu
  surface and uses the same surface type as wasi-gfx components.
- **The disk image is locked.** Two QEMUs writing the same qcow2 image corrupt
  it, and QEMU on WASI cannot lock files itself, so `lock_disk_image` takes
  the lock before QEMU starts and a new QEMU waits for the previous one.

## Where to start for common tasks

| Task | Files | Chapter |
|---|---|---|
| Change the browser UI or the pages | `src/app.rs`, `src/egui_tools.rs` | [05](05-browser-ui.md) |
| Change QEMU's options (memory, devices, disk) | `src/app.rs`, `src/headless.rs` | [05](05-browser-ui.md), [11](11-headless-and-automation.md) |
| Add a host import | `src/qemu_*.rs`, `run_wasm_module` | [13](13-adding-features.md), [host imports](reference/host-imports.md) |
| Add an environment-variable knob | the module concerned | [13](13-adding-features.md), [variables](reference/environment-variables.md) |
| JIT speed, compile errors, TB modules | `src/qemu_jit.rs` | [07](07-jit-host.md) |
| Memory growth, map-count aborts, migration | `src/wasm.rs` | [06](06-qemu-runner.md) |
| vCPU threads, races, `QEMU_SMP` | `src/qemu_threads.rs` | [08](08-threads.md) |
| Screen, keyboard, mouse | `src/qemu_gfx.rs`, `src/qemu_keymap.rs`, `src/app.rs` | [09](09-display-and-input.md) |
| Guest network, port forwarding | `src/qemu_net.rs`, `src/qemu_net/tcp.rs` | [10](10-networking.md) |
| Boot QEMU from a script or a test | `src/headless.rs` | [11](11-headless-and-automation.md) |
| Find where the time goes | `src/qemu_profile.rs` | [12](12-profiling.md) |
| A trap, a crash, a failing test | | [14](14-testing-and-debugging.md) |
| Build, run, update QEMU | | [03](03-getting-started.md) |
