# Host imports and QEMU exports

**What this covers.** The complete interface between m-browser and the
WebAssembly build of QEMU: every function m-browser provides to QEMU (its
*imports*), with its wasm type, the C declaration QEMU uses and the Rust
code behind it, and every function of QEMU's (its *exports*) that
m-browser calls. Read it when changing either side of the interface or
when a QEMU build will not link against m-browser. The QEMU-side view is
the qemu-wasi repository's docs/reference/host-interface.md.

## Overview

```text
            QEMU (qemu-system-x86_64.wasm)
  imports:  env.wasi_jit_*        -> src/qemu_jit.rs     (JIT)
            env.host_gfx_*        -> src/qemu_gfx.rs     (display, input)
            env.host_net_*        -> src/qemu_net.rs     (network)
            env.host_main_wake    -> src/qemu_net.rs     (threaded build)
            env.host_vcpu_run     -> src/qemu_threads.rs (threaded build)
            wasi.thread-spawn     -> src/qemu_threads.rs (threaded build)
            env.memory            <- src/wasm.rs         (threaded build)
            wasi_snapshot_preview1.* -> wasmtime-wasi (5 replaced in
                                        qemu_threads.rs, threaded build)
  exports:  _initialize, wasi_qemu_init, wasi_qemu_step, wasi_qemu_quit,
            wasi_qemu_jit_reset, memory, __indirect_function_table,
            wasi_vcpu_enter, wasi_thread_start (threaded build)
```

An *import* names a module (`env`, `wasi`, `wasi_snapshot_preview1`) and a
field; the Linker in `run_wasm_module` (`src/wasm.rs`) maps each to a Rust
function, in the order given in [06-qemu-runner.md](../06-qemu-runner.md).
On the QEMU side, a C function declared `extern` and defined nowhere
becomes an `env` import, because QEMU is linked with
`-Wl,--allow-undefined` (qemu-wasi's scripts/fix_build_ninja.py).

## Checking the list yourself

```sh
# from the m-browser directory: the installed build (multi-threaded)
wasm-tools print --skeleton qemu-system-x86_64.wasm \
    | grep -E '^\s*\((import|export)'
# from the qemu-wasi directory: either build (the files have no extension)
wasm-tools print --skeleton qemu-build/qemu-system-x86_64 \
    | grep -E '^\s*\((import|export)'
wasm-tools print --skeleton qemu-build-threads/qemu-system-x86_64 \
    | grep -E '^\s*\((import|export)'
```

The tables below were checked against both builds in this way.

## Conventions

- Wasm types: `i32` and `i64` are 32- and 64-bit integers; `(i32, i32) ->
  i32` is a function of two `i32` returning one. Rust's `u32` and C's
  `uint32_t` are the same wasm `i32`.
- Pointers are byte offsets into QEMU's linear memory. The host reads and
  writes them with `with_guest`/`with_guest_mut` (`src/qemu_jit.rs`): a
  bad range gets an error value (and usually a log line), never a trap.
  Strings come with a length; they are not NUL-terminated. WASI functions
  return an errno (0 for success) and write results through pointers.
- Builds: *both* means the current single-threaded (`make`) and
  multi-threaded (`make THREADS=1`) builds; *threaded* only the latter;
  *none* means defined for older builds, imported by no current one.
- C declarations are quoted from qemu-wasi (`qemu-src/tcg/wasi_jit.h`,
  `display/wasi_gfx_display.c`, `qemu-src/net/wasi-user.c.inc`,
  `qemu-src/util/main-loop.c`, `qemu-src/accel/tcg/tcg-accel-ops-mttcg.c`,
  all as `extern`) and from wasi-libc.

## JIT: env.wasi_jit_* (src/qemu_jit.rs, add_jit_to_linker)

Details in [07-jit-host.md](../07-jit-host.md).

| Import | Wasm type | C declaration | Rust | Builds | Semantics |
|---|---|---|---|---|---|
| `env.wasi_jit_compile` | `(i32, i32, i32, i32) -> i32` | `uint32_t wasi_jit_compile(const uint8_t *wasm_bytes, uint32_t wasm_bytes_len, const uint32_t *helper_func_indices, uint32_t helper_func_count)` | closure calling `jit_load(.., LoadMode::Compile)` | both | Compile the TB module at `wasm_bytes` unless cached, instantiate it, return its function table index; 0 on failure |
| `env.wasi_jit_load_cached` | `(i32, i32, i32, i32) -> i32` | same parameters | `jit_load(.., LoadMode::CachedOnly)` | both | The same, but 0 instead of compiling |
| `env.wasi_jit_load_async` | `(i32, i32, i32, i32) -> i32` | same parameters; `#define WASI_JIT_FAILED 0xffffffffu` | `jit_load(.., LoadMode::Async)` | both | The same, but compile on a background thread and return 0 meanwhile; `0xffffffff` (`JIT_LOAD_FAILED`) once the compile failed |
| `env.wasi_jit_call` | `(i32, i32) -> i32` | `uint32_t wasi_jit_call(uint32_t module_handle, uint32_t ctx_ptr)` | closure | none | Call a TB from the host; on a trap, log it, clear `ctx->tb_ptr`, return 0 |
| `env.wasi_jit_drop` | `(i32) -> ()` | `void wasi_jit_drop(uint32_t module_handle)` | closure | none (declared, never called) | Nothing; logs unknown handles |
| `env.wasi_jit_take_pending_exception` | `() -> i32` | `int wasi_jit_take_pending_exception(void)` | closure | none | 1 (and clear the flag) if a legacy helper trampoline caught an exception, else 0 |

## Display and input: env.host_gfx_* (src/qemu_gfx.rs, add_gfx_to_linker)

Details in [09-display-and-input.md](../09-display-and-input.md).

| Import | Wasm type | C declaration | Rust | Builds | Semantics |
|---|---|---|---|---|---|
| `env.host_gfx_init` | `(i32, i32) -> ()` | `void host_gfx_init(int32_t width, int32_t height)` | closure | both | The display starts at this size: create the softbuffer surface on QEMU's window, if there is one |
| `env.host_gfx_update_framebuffer` | `(i32, i32, i32, i32, i32) -> ()` | `void host_gfx_update_framebuffer(const uint8_t *data_ptr, int32_t data_len, int32_t width, int32_t height, int32_t stride)` | closure | both | Show `height` rows of `width` BGRA pixels at `data_ptr`, `stride` bytes apart (headless: a PNG now and then, `QEMU_SNAPSHOT`) |
| `env.host_gfx_resize` | `(i32, i32) -> ()` | `void host_gfx_resize(int32_t width, int32_t height)` | closure | both | The display's size changed |
| `env.host_gfx_poll_events` | `(i32, i32) -> i32` | `int32_t host_gfx_poll_events(int32_t *event_buf, int32_t max_events)` | closure capturing the `GfxEventQueue` | both | Move up to `max_events` queued input events into `event_buf`, 12 bytes each (`type, code, value` as `i32`); return how many |

The queue is filled by the browser (`src/app.rs`) or the headless runner.

## Network: env.host_net_*, host_main_wake (src/qemu_net.rs)

Details in [10-networking.md](../10-networking.md). Each closure, in
`add_net_to_linker`, calls a method of the `VirtualNet` the Linker owns.

| Import | Wasm type | C declaration | Rust | Builds | Semantics |
|---|---|---|---|---|---|
| `env.host_net_send` | `(i32, i32) -> i32` | `int32_t host_net_send(const void *frame, int32_t len)` | `VirtualNet::from_guest` | both | Pass one Ethernet frame from the guest; returns `len`, or -1 if out of bounds (QEMU ignores it) |
| `env.host_net_recv` | `(i32, i32) -> i32` | `int32_t host_net_recv(void *buf, int32_t size)` | `VirtualNet::to_guest` | both | Copy the next frame for the guest into `buf`, return its length; 0 if none (a frame larger than `size` is dropped) |
| `env.host_net_wait` | `(i64) -> ()` | `void host_net_wait(int64_t timeout_ns)` | `VirtualNet::wait_for_frame` | both | Sleep up to `timeout_ns`, until a frame for the guest arrives or `host_main_wake` is called |
| `env.host_net_hostfwd` | `(i32, i32) -> i32` | `int32_t host_net_hostfwd(const char *rule, int32_t len)` | `VirtualNet::add_forward` | both | Forward a host TCP port into the guest (`hostfwd` rule); 0, or -1 after printing why not |
| `env.host_main_wake` | `() -> ()` | `void host_main_wake(void)` | `VirtualNet::wake_main` | threaded | End the main thread's current or next `host_net_wait`; called by other threads that need the main loop (`wasi_main_wake`) |

## Threads (src/qemu_threads.rs, add_threads_to_linker; src/wasm.rs)

Details in [08-threads.md](../08-threads.md).

| Import | Wasm type | C declaration | Rust | Builds | Semantics |
|---|---|---|---|---|---|
| `wasi.thread-spawn` | `(i32) -> i32` | wasi-libc's `int32_t __wasi_thread_spawn(void *start_arg)`, called by `pthread_create()` | closure calling `QemuThreads::spawn` | threaded | Start OS thread `qemu-<id>`, which runs `wasi_thread_start(id, start_arg)` in a new instance; return the id (> 0), or -6 (EAGAIN) |
| `env.host_vcpu_run` | `(i32, i32, i32) -> ()` | `void host_vcpu_run(int32_t cpu_index, uint32_t stack_top, uint32_t tls_base)` | closure calling `QemuThreads::run_vcpu` | threaded | Run this vCPU through `wasi_vcpu_enter` in Stores of its own until it is unplugged |
| `env.memory` | `(memory 256 65536 shared)` | none: the threaded build is linked with `-Wl,--import-memory` (scripts/fix_build_ninja.py) | `SharedMemory::new` and `linker.define` in `run_wasm_module` | threaded | QEMU's linear memory, shared by all its threads: 16 MiB at first, up to 4 GiB |

## WASI: wasi_snapshot_preview1 (wasmtime-wasi)

`wasmtime_wasi::p1::add_to_linker_sync` in `run_wasm_module` provides
all of WASI preview 1, and each function finds its context through
`QemuHostState::wasi_ctx()`. The table lists what QEMU imports. On the C
side these are wasi-libc's `__wasi_<name>()` functions (`<wasi/api.h>`),
called by libc (`open`, `read`, `clock_gettime`, `nanosleep`, ...) and,
for the arguments, directly by `wasi_qemu_init`. In the module they appear
as `__imported_wasi_snapshot_preview1_<name>`.

| Functions | Wasm type | Builds | Threaded build |
|---|---|---|---|
| `args_get`, `args_sizes_get`, `environ_get`, `environ_sizes_get` | `(i32, i32) -> i32` | both | wasmtime-wasi |
| `clock_time_get` | `(i32, i64, i32) -> i32` | both | **replaced** (`clock_now`) |
| `clock_res_get` | `(i32, i32) -> i32` | none | **replaced** (defined, not imported) |
| `fd_close`, `fd_sync` | `(i32) -> i32` | both | wasmtime-wasi |
| `fd_fdstat_get`, `fd_fdstat_set_flags`, `fd_filestat_get`, `fd_prestat_get`, `fd_tell` | `(i32, i32) -> i32` | both | wasmtime-wasi |
| `fd_filestat_set_size` | `(i32, i64) -> i32` | both | wasmtime-wasi |
| `fd_pread`, `fd_pwrite`, `fd_readdir` | `(i32, i32, i32, i64, i32) -> i32` | both | wasmtime-wasi |
| `fd_prestat_dir_name`, `path_create_directory`, `path_remove_directory`, `path_unlink_file` | `(i32, i32, i32) -> i32` | both | wasmtime-wasi |
| `fd_read`, `fd_write` | `(i32, i32, i32, i32) -> i32` | both | wasmtime-wasi |
| `fd_seek` | `(i32, i64, i32, i32) -> i32` | both | wasmtime-wasi |
| `path_filestat_get` | `(i32, i32, i32, i32, i32) -> i32` | both | wasmtime-wasi |
| `path_open` | `(i32, i32, i32, i32, i32, i64, i64, i32, i32) -> i32` | both | wasmtime-wasi |
| `path_readlink` | `(i32, i32, i32, i32, i32, i32) -> i32` | both | wasmtime-wasi |
| `poll_oneoff` | `(i32, i32, i32, i32) -> i32` | both | **replaced**: clock subscriptions only, `ENOTSUP` for file descriptors |
| `proc_exit` | `(i32) -> ()` | both | wasmtime-wasi; becomes the `I32Exit` error ([06-qemu-runner.md](../06-qemu-runner.md), [08-threads.md](../08-threads.md)) |
| `random_get` | `(i32, i32) -> i32` | both | **replaced** (`rand::rng()`) |
| `sched_yield` | `() -> i32` | threaded | **replaced** (`std::thread::yield_now()`) |

The replacements (`add_threads_to_linker`) exist so that clocks, random
numbers, yields and sleeps never take the lock of the shared WASI context;
in the single-threaded build wasmtime-wasi's own versions are used.

## QEMU's exports, as m-browser uses them

| Export | Wasm type | Builds | Called by | When, and what it does |
|---|---|---|---|---|
| `_initialize` | `() -> ()` | both | `run_qemu_reactor` | Once, first: wasi-libc's start-up, QEMU's C constructors |
| `wasi_qemu_init` | `() -> i32` | both | `run_qemu_reactor` | Once: read the arguments, create the machine. 0 = running, 1 = arguments unreadable; `exit()` on errors |
| `wasi_qemu_step` | `() -> i32` | both | `run_qemu_reactor` | In a loop: one main-loop iteration. -1 = go on, otherwise the exit status |
| `wasi_qemu_quit` | `() -> ()` | both | `run_qemu_reactor` | Once, when asked to stop: request a clean shutdown (the following steps perform it) |
| `wasi_qemu_jit_reset` | `() -> ()` | both | `migrate_store` | After each store migration (single-threaded build only): forget every TB handle |
| `wasi_vcpu_enter` | `(i32, i32, i32) -> i32` | threaded | `QemuThreads::run_vcpu` | Repeatedly, per vCPU: `(stack_top, tls_base, flags)`, flag 1 = new Store; one vCPU loop iteration; 0 = go on, otherwise unplugged |
| `wasi_thread_start` | `(i32, i32) -> ()` | threaded | `QemuThreads::spawn` | Once per new thread: `(id, start_arg)`; runs the thread's function |
| `memory` | memory | both | wasmtime-wasi's functions; `run_wasm_module`, `migrate_store` | wasmtime-wasi finds the caller's memory through this export (which is why the threaded build re-exports its imported memory, `--export-memory`). Single-threaded build: also kept as `guest_memory` and copied at each migration |
| `__indirect_function_table` | `funcref` table | both | `run_wasm_module`, `migrate_store`, `QemuThreads::instantiate`, `jit_load` | Kept as `guest_table`; each new TB instance's `start` is appended, its index being the handle (linked with `--export-table --growable-table`) |
| `_start` | `() -> ()` | none (old command builds) | `run_wasm_module` | Only if `wasi_qemu_step` is missing: runs QEMU to the end |

## Changing the interface

Both repositories change together. A QEMU build that imports something
this m-browser lacks, or with another wasm type, fails to instantiate with
an error naming the import; extra definitions in m-browser are harmless,
which is why the legacy JIT functions stay. Add a new function in the
`add_*_to_linker` function of its area, then update this file and the
qemu-wasi repository's docs/reference/host-interface.md; the procedure is
in [13-adding-features.md](../13-adding-features.md).
