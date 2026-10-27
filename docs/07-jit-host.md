# The JIT host (src/qemu_jit.rs)

**What this covers.** How m-browser turns the guest code QEMU translates
into machine code: the `env.wasi_jit_*` functions QEMU calls, the module
cache and background compiler behind them, how compiled code is handed
back to QEMU, and how host functions read QEMU's memory. Read it before
changing `src/qemu_jit.rs`, when tuning JIT performance, or when compiled
guest code misbehaves. QEMU's half of the JIT is described in the qemu-wasi
repository's docs/07-tcg-backend.md.

## Overview

QEMU cannot compile WebAssembly itself (WASI has no API for it), so it
hands each hot piece of translated guest code, already a small wasm
module, to the host. m-browser compiles it with Cranelift (wasmtime's
compiler), instantiates it in QEMU's Store (wasmtime's container for
running code) with QEMU's own memory and function table as imports, puts
its entry function into QEMU's function table, and returns the index. QEMU
then calls the code like any C function pointer.

```text
QEMU (tcg/wasm32.c)                      m-browser: jit_load (src/qemu_jit.rs)
TB = wasm module bytes in QEMU's memory
 cold: interpreted, run count + 1
 warm (QEMU_JIT_WARM runs):
   wasi_jit_load_cached(bytes) ------->  1. same bytes instantiated in this
 hot (from QEMU_JIT_HOT runs, every         Store (by_key)? -> its handle
      16 runs until it has a handle):    2. module compiled (JitCache)?
   wasi_jit_load_async(bytes) -------->     if not: CachedOnly -> 0
 QEMU_JIT_ASYNC=0, or not interpretable:            Async -> 0, compile
   wasi_jit_compile(bytes) ----------->               on a qemu-jit thread
                                                    Compile -> compile now
                                         3. Instance::new(module,
                                              [QEMU memory, QEMU table])
 handle <------------------------------  4. table.grow(1, start) = handle
 call_indirect handle(&ctx): runs the machine code Cranelift made
```

## Words used in this document

| Term | Meaning |
|---|---|
| guest, host, vCPU | The emulated x86-64 PC, the real machine (here: m-browser), and one of the guest's virtual CPUs |
| TCG, TB | QEMU's "Tiny Code Generator" translates guest instructions in short runs called translation blocks (TBs). The wasm32 backend emits each TB as a wasm module with one function, `start` |
| JIT | Just-in-time compilation: making machine code while the program runs |
| interpreter tier | QEMU's interpreter for TB modules (`tcg/wasm32/wasi-interp.c.inc` in qemu-wasi's patched `qemu-src/`), used until a TB is hot. Compiling costs about 1 ms per TB, too much for code that runs a few times |
| Cranelift, Module, Instance, Store | wasmtime's compiler; a compiled program; a running copy of it; the container of instances, freed only as a whole ([06-qemu-runner.md](06-qemu-runner.md)) |
| function table, call_indirect | The array C function pointers index into, and the wasm instruction that calls through it |
| handle | What the `wasi_jit_*` loads return: the index of the TB's `start` function in QEMU's function table. 0 means "none" |
| helper | A C function in QEMU that TB code calls for complex work (and that may `longjmp` out) |
| memory mapping | A region of the process's address space; each compiled module holds about two, and Linux caps their number |

## QEMU's side, briefly

1. TCG translates guest code into a TB; the wasm32 backend writes it as a
   module into QEMU's code buffer: one function `start(ctx) -> i32`, two
   imports, `env.buffer` (QEMU's memory) and `env.table` (QEMU's function
   table), and helpers called with `call_indirect` through that table.
2. `tcg_qemu_tb_exec` runs TBs. A TB without a handle for this vCPU is
   interpreted and its run count grows.
3. At `QEMU_JIT_WARM` runs (default 50) QEMU asks `wasi_jit_load_cached`:
   is this module still compiled (from before a TB buffer flush, say)?
4. From `QEMU_JIT_HOT` runs (default 5000) it asks `wasi_jit_load_async`
   every 16 runs (`JIT_ASYNC_RETRY`) until it gets a handle, interpreting
   meanwhile. With `QEMU_JIT_ASYNC=0` or `QEMU_JIT_HOT=0`, and for TBs the
   interpreter cannot run, it calls `wasi_jit_compile` and waits.
5. `compile_and_cache_tb` copies the module's parts into one buffer, makes
   the call, and records the handle in the TB (one slot per vCPU) and in a
   per-thread list, so that all can be forgotten at once.
6. With a handle, QEMU calls the TB through its function table: a plain
   wasm-to-wasm call. A helper that `longjmp`s (`cpu_loop_exit`, for a
   guest exception) throws a wasm exception that unwinds through the TB
   straight to `cpu_exec`'s `setjmp`, because no host frame is in between.

`start(ctx)` gets a pointer to QEMU's per-thread `wasmContext` (CPU state,
stack, current TB) and sets `ctx->tb_ptr` to the next TB to run, or to 0 to
return to QEMU's CPU loop.

## The imports

| Import | Used by current QEMU builds | Behaviour |
|---|---|---|
| `wasi_jit_compile(wasm, len, helpers, n)` | yes | `jit_load(Compile)`: compile now if needed; handle, or 0 on failure |
| `wasi_jit_load_cached(...)` | yes | `jit_load(CachedOnly)`: 0 unless the module is compiled already |
| `wasi_jit_load_async(...)` | yes | `jit_load(Async)`: 0 while it compiles in the background; `JIT_LOAD_FAILED` (`u32::MAX`) once compiling failed |
| `wasi_jit_call(handle, ctx)` | no, legacy | Run a TB from the host; on a trap, log it once per handle, clear `ctx->tb_ptr` (offset 8) so QEMU's loop stops, return 0 |
| `wasi_jit_drop(handle)` | no (declared in tcg/wasi_jit.h, never called) | Nothing, except a log line for unknown handles |
| `wasi_jit_take_pending_exception()` | no, legacy | Return 1 and clear `pending_exception` if it was set |

`wasm`/`len` locate the module's bytes in QEMU's memory; `helpers`/`n` are
an array of n `u32` function table indices of the helpers it calls, used
only for legacy modules. Unused definitions cost nothing, so the legacy
ones stay for older QEMU builds. Exact signatures:
[reference/host-imports.md](reference/host-imports.md).

## jit_load, step by step

1. Start the profiler's timer (`HostWork::JitTotal`); fetch `guest_memory`
   (log `guest memory not set` and return 0 if missing).
2. Look the bytes up in `by_key` while they are still in QEMU's memory.
   Found: an instance of this very module is in this Store, so count
   `stats_shared` and return its handle. This is the fast path: no copy,
   no lock.
3. Otherwise copy the bytes into an `Arc<[u8]>` (out of bounds: log, 0).
4. Lock the shared `JitCache`, and `collect_background()`: move finished
   background compiles into the module cache, rejected ones into `failed`.
5. Get the Module according to the `LoadMode` (next section), or return.
6. Unlock. Find QEMU's function table (`guest_table`, else the caller's
   `__indirect_function_table` export).
7. Instantiate: directly with `[memory, table]` if the module imports only
   `env.buffer` and `env.table`, else through `legacy_linker`.
8. Get the `start` export with type `(i32) -> (i32)`.
9. `guest_table.grow(1, start)`: the new slot's index is the handle.
10. Record it: `instances_in_store += 1`, `modules[handle]`,
    `by_key[bytes] = handle`. Return the handle.

Any failure in steps 6 to 9 is logged (`[qemu-jit] compile: ...`) and
returns 0.

**Pitfall:** in `Async` mode a 0 means "not ready", so QEMU asks again 16
runs later. A failure to instantiate (for example at the Store's instance
limit) therefore repeats, with a log line each time; only compile failures
become `JIT_LOAD_FAILED`.

## LoadMode

| Mode | Import | Module compiled already | Not compiled yet |
|---|---|---|---|
| `Compile` | `wasi_jit_compile` | Instantiate | Compile now on QEMU's thread (`get_or_compile`), cache, instantiate. On error, save the bytes to `$TMPDIR/qemu-jit-failed.wasm` (default `/tmp`), log, return 0 |
| `CachedOnly` | `wasi_jit_load_cached` | Instantiate | Return 0 |
| `Async` | `wasi_jit_load_async` | Instantiate | `JIT_LOAD_FAILED` if in `failed`; otherwise queue it (`compile_in_background`, unless pending) and return 0 |

Every hit in the module cache counts in `stats_cache_hits`.

## JitCache: compiled modules, shared by all Stores

A `Module` belongs to the Engine, not to a Store, so one compiled TB can be
instantiated in any Store: after a store migration, in the next Store; in
the multi-threaded build, in every vCPU's Store. `JitCache` (one per QEMU
run, behind `Arc<Mutex<...>>`) holds them:

| Field | Purpose |
|---|---|
| `engine` | For compiling |
| `module_cache`, `module_cache_order` | Compiled modules keyed by their wasm bytes, and their insertion order: a FIFO (a hit does not refresh an entry) |
| `compiler` | The `BackgroundCompiler`, started on the first `Async` miss |
| `stats_compiled`, `stats_compile_time`, `stats_by_size` | Synchronous compiles: count, time, and count/time/bytes by module size (<512 B, <1 KiB, <2 KiB, <4 KiB, larger) |
| `stats_cache_hits` | Loads served by the module cache |

`cache_module` drops the oldest entry once `MODULE_CACHE_MAX` (18,000)
modules are cached. The bound exists because every live module holds
about two memory mappings, which count against `vm.max_map_count` together
with the modules still used by a Store (see
[06-qemu-runner.md](06-qemu-runner.md)). `shrink_module_cache(keep)` drops
the oldest down to `keep`; the runner halves the cache when mappings stay
high after a move. An evicted module stays mapped as long as an instance
in some Store uses it.

Keys are the module bytes (`BytesMap`, a `HashMap<Arc<[u8]>, _>` with
FxHash from the rustc-hash crate). Keys are 1 to 4 KB and hashed on every
load, shared ones included; FxHash is several times faster than Rust's
default SipHash there, and resistance to crafted keys does not matter (QEMU
makes them).

## BackgroundCompiler

```text
QEMU's thread (jit_load, Async miss)       qemu-jit-0 .. qemu-jit-N-1
  compile_in_background(bytes)
    pending += bytes ---- jobs channel ----> Module::new(bytes) (Cranelift)
                                                  |
  next jit_load: collect_background() <-- done ---+  (bytes, Some/None)
    Some: module cache, pending -= bytes
    None: failed += bytes, pending -= bytes
```

- Threads: `QEMU_JIT_THREADS`, else a quarter of the host's CPUs, clamped
  to 1..4 (at least 1 either way). Log:
  `[qemu-jit] compiling TBs in the background on N threads`.
- `jobs`: an unbounded channel of module bytes. The workers share its
  receiver behind a mutex, held only while waiting for the next job.
- `done`: finished compiles, collected by QEMU's thread(s) during
  `jit_load`, under the cache lock. A finished module waits there until
  the next load request, which QEMU makes at most 16 TB runs later.
- `pending`: queued or compiling, so nothing is queued twice. `failed`:
  modules Cranelift rejected (log: `background compile failed`); QEMU
  keeps interpreting those TBs.
- `compiled`, `compile_ns`: atomic counters for the statistics.

Workers exit when the `JitCache`, which owns the channel's sender, is
dropped. Compiling in the background made QEMU about 7% faster and removed
the ~1 ms stalls of QEMU's thread at each compile. `QEMU_JIT_ASYNC=0`
(read by QEMU) turns it off.

## QemuJitState: instances in one Store

| Field | Purpose |
|---|---|
| `cache` | The shared `JitCache` |
| `modules` | Handle to `CompiledTB { instance, start_func }`, for the legacy `wasi_jit_call` and the check in `wasi_jit_drop` |
| `by_key` | Module bytes to handle, for this Store |
| `instances_in_store` | TB instances created in this Store; the runner moves to a fresh Store when it is high |
| `stats_shared` | Loads served by `by_key` |

**Why identical TBs can share one instance and one table slot.** A TB
module has no state of its own: TCG's registers are locals of its function,
and its memory and table are QEMU's. Its code is also position independent:
addresses near the TB are computed from `ctx->tb_ptr` instead of being
constants. So any TB with the same bytes behaves the same, whatever
address it was translated at. After a TB buffer flush, retranslated code
gets its old handle back (at its `QEMU_JIT_WARM`-th run) for a hash
lookup, and instances, which only a Store migration frees, pile up far
more slowly.

That is also why `wasi_jit_drop` frees nothing: other TBs may use the same
handle, and a slot cannot be reused while any TB in QEMU might still hold
its index. `reset_for_new_store()` clears `modules`, `by_key` and
`instances_in_store` when the runner moves QEMU to a fresh Store.

## Instantiation without a Linker, and legacy_linker

Current TB modules import exactly `env.buffer` and `env.table`. `jit_load`
then builds the import list itself (`GuestMem::to_extern()` and the table,
in the module's import order) and calls `Instance::new`: building a Linker
for each of thousands of TBs would cost far more.

Modules from older QEMU builds import each helper as `helper.N`.
`legacy_linker` builds a Linker per module with `env.buffer`, `env.table`
and, for each helper, a host trampoline (`Func::new`) that looks up
`helpers[N]` in QEMU's table and calls it; `read_helper_indices` reads the
array. A wasm exception cannot cross the trampoline's host frame, so it
sets `pending_exception` and fails; QEMU then called
`wasi_jit_take_pending_exception` and re-raised `cpu_loop_exit` in C. Every
helper call, and every TB call (`wasi_jit_call`), went through the host:
the current design avoids all of it.

## Table slots and handles

- `guest_table.grow(1, start)` appends one slot and returns the old size,
  which is the new slot's index and the handle. It is never 0: QEMU's
  table starts with its own entries (about 8,900). QEMU is linked with
  `--export-table --growable-table` (qemu-wasi's scripts/fix_build_ninja.py)
  so that the host can do this.
- Slots are never freed or reused within a Store: the table grows by one
  per distinct module. A new Store starts from QEMU's own entries again.
- A handle is valid only in the Store, and table, that made it. After a
  move, every handle is stale (`wasi_qemu_jit_reset`). In the
  multi-threaded build each vCPU has its own Store and table, and QEMU
  keeps one handle per vCPU in each TB (8 slots: at most 8 vCPUs).

## Reading QEMU's memory: GuestMem, with_guest, with_guest_mut

`QemuHostState::guest_memory` is a `GuestMem`: `Plain(Memory)`, QEMU's own
memory, or `Shared(SharedMemory)` in the multi-threaded build. Host
functions access it with:

```rust
with_guest(&caller, ptr, len, |bytes: &[u8]| ...)              // -> Option<R>
with_guest_mut(&mut caller, ptr, len, |bytes: &mut [u8]| ...)  // -> Option<R>
```

`None` means the range is outside the memory. They take closures because
a plain memory's bytes are borrowed from the Store, while a shared
memory's bytes belong to no Store; a closure works for both and keeps the
borrow short. With shared memory, other threads may write the bytes while
the closure runs: `with_guest` callers must accept an inconsistent snapshot
(a torn frame, say), and `with_guest_mut` relies on QEMU handing the host
buffers no other thread touches meanwhile.

**Pitfall:** copy what you need inside the closure, never keep the slice,
and never call back into wasm from it. Convert QEMU's pointers with
`ptr as u32 as usize`: a negative `i32` would otherwise sign-extend.

## QemuHostState: the data of every QEMU Store

| Field | Purpose |
|---|---|
| `wasi` | `WasiSlot::Own(WasiP1Ctx)` in the single-threaded build; `WasiSlot::Shared(Arc<SharedWasi>)`, one context for all threads, in the other |
| `wasi_locked` | Whether this Store holds the shared context's lock |
| `jit` | `QemuJitState` |
| `gfx` | `Arc<Mutex<QemuGfxState>>`, the display ([09-display-and-input.md](09-display-and-input.md)) |
| `guest_memory`, `guest_table` | QEMU's memory and function table in this Store, set after instantiation |
| `pending_exception` | Legacy helper trampolines only |
| `limits` | `qemu_store_limits()`: up to 150,000 instances per Store instead of wasmtime's 10,000 |

`wasi_ctx()` is how wasmtime-wasi's functions get the WASI context: the
own one, or the shared one after taking its lock, which a call hook
releases when the WASI function returns (`release_wasi()`;
[08-threads.md](08-threads.md)).

## Looking at TB modules

- `QEMU_JIT_DUMP=<dir>[,<n>]` (m-browser): `dump_tb_module` writes every
  n-th newly compiled module (default every 100th) to
  `<dir>/tb-0000100.wasm` and so on. Cache hits are not dumped.
- `QEMU_TB_DUMP=<pc>,<dir>` (QEMU): writes the module of the TB at guest
  address `<pc>` (hex) to `<dir>/tb_<pc>.wasm` when QEMU asks the host to
  load it.
- A module Cranelift rejects in `Compile` mode is saved to
  `$TMPDIR/qemu-jit-failed.wasm`, overwritten each time. Background
  failures are only logged: reproduce with `QEMU_JIT_ASYNC=0`.

**Pitfall:** with background compiling, the modules collected in one
`collect_background()` call usually get the same number, so dumps
overwrite each other and fewer than every n-th are kept. For exact
sampling add `QEMU_JIT_ASYNC=0`.

For example, from the m-browser directory (GUI or headless both work):

```sh
QEMU_JIT_DUMP=/tmp/tbs,500 ./target/release/m-browser
wasm-tools print /tmp/tbs/tb-0000500.wasm | less    # the text format
wasm-tools validate /tmp/tbs/tb-0000500.wasm         # is it valid wasm?
```

What to expect, from the generator (`wasm_finalize_module` in
`tcg/wasm32/tcg-wasm32-helpers.c.inc`, qemu-wasi's `qemu-src/`): type 0 is
`(func (param i32) (result i32))`, then one type per helper; the imports
`env.buffer` (a memory, `shared` in the multi-threaded build) and
`env.table`; one function, exported as `start`, with 2 `i32` and 30 `i64`
locals (temporaries and TCG's registers); a `loop` whose blocks are guarded
by a block-pointer local, and helper calls as `i32.const <table index>`
then `call_indirect`.

## Tuning knobs

| Knob | Read by | Default | Effect |
|---|---|---|---|
| `QEMU_JIT_HOT` | QEMU | 5000 | Interpreted runs before a TB is compiled; 0 compiles every TB at its first run, synchronously |
| `QEMU_JIT_WARM` | QEMU | 50 | Runs before QEMU asks for a module the host still has |
| `QEMU_JIT_ASYNC=0` | QEMU | on | Compile synchronously, on QEMU's thread |
| `QEMU_JIT_THREADS` | m-browser | CPUs / 4, 1..4 | Background compiler threads |
| `QEMU_WASM_OPT` | m-browser | unset | `speed`: optimize TBs too; `none`: QEMU unoptimized ([06-qemu-runner.md](06-qemu-runner.md)) |
| `QEMU_MIGRATE_AFTER` | m-browser | 100000 | TB instances per Store before a move |
| `QEMU_JIT_STATS`, `QEMU_JIT_DUMP` | m-browser | off | Statistics every minute; module dumps |
| `MODULE_CACHE_MAX` | constant | 18,000 | Compiled modules kept |
| `-accel tcg,tb-size=<MiB>` | QEMU option | 256 in the GUI | Translated-code buffer. Too small: frequent `[qemu-wasi] TB buffer flush` and recompiles |

All variables are listed in
[reference/environment-variables.md](reference/environment-variables.md);
measuring their effect is described in [12-profiling.md](12-profiling.md).

**Pitfall:** keep the start of `jit_load` cheap. QEMU asks again and again
(every 16 runs of a hot TB until it is ready, and again after every flush
or move), so the `by_key` lookup is on a hot path; never hold the
`JitCache` lock while wasm code runs.
