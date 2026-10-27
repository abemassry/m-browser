# The QEMU runner (src/wasm.rs)

**What this covers.** How m-browser loads the WebAssembly build of QEMU and
keeps it running: the wasmtime settings, the host functions QEMU is linked
to, QEMU's sandboxed view of the host, the loop that drives QEMU, the disk
cache of QEMU's compiled code, and the "store migration" that frees the code
compiled for the guest (the emulated PC). Read it before changing
`src/wasm.rs`, or when QEMU will not start, will not stop, or the process
runs out of memory mappings. The JIT is in [07-jit-host.md](07-jit-host.md),
the multi-threaded build in [08-threads.md](08-threads.md).

## Overview

`run_wasm_module` is the single entry point for QEMU. The browser calls it
on a thread of its own when a page is a core wasm module (`src/app.rs`,
[05-browser-ui.md](05-browser-ui.md)); the headless runner calls it for
`m-browser --qemu-headless` (`src/headless.rs`,
[11-headless-and-automation.md](11-headless-and-automation.md)). It returns
once QEMU has exited.

```text
app.rs (QEMU's own thread)            headless.rs (--qemu-headless)
                   \                     /
  run_wasm_module(wasm_path, qemu_args, preopen_dirs,
                  surface, event_queue, stop)
    1. Engine settings; load_qemu_module (compiled once, cached on disk)
    2. does the module import env.memory?  -> multi-threaded build
    3. Linker: WASI, JIT, display, network [, threads]
    4. WASI context: arguments (+ -smp), QEMU_*/WASI_* variables, preopens
          |                                       |
  single-threaded build: one Store       multi-threaded build: QemuThreads
          |                                       |
          +---------> run_qemu_reactor <----------+
                      _initialize, wasi_qemu_init, wasi_qemu_step ...
                      (+ migrate_store, single-threaded build only)
```

## Words used in this document

| Term | Meaning |
|---|---|
| core module | A plain WebAssembly program. QEMU is one (`qemu-system-x86_64.wasm`, about 40 MB); wasi-gfx demos are *components*, a newer packaging run by `Wasm` (last section) |
| wasmtime, Cranelift | The Rust library that compiles and runs WebAssembly (version 41), and its compiler from WebAssembly to machine code |
| Engine, Module | wasmtime's compiler settings; a compiled program, which belongs to the Engine and can be used in any Store |
| Store | A container for running instances and the host's data for them. wasmtime frees what is in a Store only when the whole Store is dropped |
| Instance | A running copy of a Module: its globals, its function table, its memory (unless imported) |
| linear memory | An instance's flat byte array. QEMU's C pointers are offsets into it, and the guest's RAM lives in it |
| function table | The array that C function pointers index (`__indirect_function_table`) |
| import, export, Linker | What a module needs from the host, and what it offers. A Linker maps import names to Rust functions |
| WASI, preopen | The standard "system call" interface for WebAssembly (files, clocks, arguments; here *preview 1*, from the wasmtime-wasi crate). A preopen is a host directory the sandbox may use, under a name the host picks (`/data`) |
| reactor | A module without a `main`: the host calls its exported functions when it wants (QEMU runs one main-loop iteration per call) |
| TB | Translation block: a short run of guest instructions QEMU translated. Each hot TB becomes a tiny wasm module ([07-jit-host.md](07-jit-host.md)) |
| trap | A WebAssembly runtime error, such as `abort()` executing `unreachable`; the Rust caller receives it as an error |
| memory mapping | A region of the process's address space, one line of `/proc/self/maps`. Linux caps their number at `vm.max_map_count` (65,530 by default) |

## run_wasm_module, step by step

### 1. Engine settings and loading

| Setting | Why |
|---|---|
| `wasm_exceptions(true)` | QEMU leaves deeply nested code with `setjmp`/`longjmp` (`cpu_loop_exit()` abandons a guest instruction from inside a helper). qemu-wasi's scripts/configure.sh compiles it with `-mllvm -wasm-enable-sjlj -mllvm -wasm-enable-eh -mllvm --wasm-use-legacy-eh=false`, which makes them standard wasm exceptions (`try_table`, `throw`). Without this setting the module fails validation |
| `wasm_backtrace_details(Enable)` | Trap reports name source files and lines, from QEMU's DWARF debug info (the build has it). `QEMU_PROFILE` turns it off again ([12-profiling.md](12-profiling.md)) |
| `cranelift_opt_level(OptLevel::None)` | The engine compiles hundreds of thousands of tiny TB modules, about 1 ms each unoptimized: compile time matters more than code quality. QEMU's own code is optimized anyway |
| `QEMU_WASM_OPT=speed` | Experiment: `OptLevel::Speed` for everything, TBs included |
| `qemu_profile::configure` | With `QEMU_PROFILE` only: epoch interruption, for sampling |
| `shared_memory(true)` | Allows the shared memory the multi-threaded build imports; harmless otherwise |

Then `load_qemu_module` ([below](#qemus-compiled-code-cache-load_qemu_module))
provides QEMU's compiled Module.

### 2. Which build is it?

QEMU built with `make THREADS=1` imports its memory (`env.memory`, shared
by all its threads) instead of having its own. If that import is there,
`threaded` is true (log: `Multi-threaded QEMU: one thread per vCPU`).

### 3. The Linker, in this order

| # | Call | Adds |
|---|---|---|
| 1 | `wasmtime_wasi::p1::add_to_linker_sync(&mut linker, \|s\| s.wasi_ctx())` | Every `wasi_snapshot_preview1` function. Each gets the WASI context from `QemuHostState::wasi_ctx()`, which takes the shared lock in the threaded build |
| 2 | `qemu_jit::add_jit_to_linker` | `env.wasi_jit_*` ([07-jit-host.md](07-jit-host.md)) |
| 3 | `qemu_gfx::add_gfx_to_linker(&mut linker, event_queue)` | `env.host_gfx_*`; the input queue is captured here, so it survives store migrations ([09-display-and-input.md](09-display-and-input.md)) |
| 4 | `qemu_net::add_net_to_linker` | `env.host_net_*`, `env.host_main_wake`. Starts the network thread at once; the network lives as long as the Linker ([10-networking.md](10-networking.md)) |
| 5 | threaded only: `qemu_threads::add_threads_to_linker` | `wasi.thread-spawn`, `env.host_vcpu_run`, and replacements for five WASI functions. Must follow step 1: it shadows (redefines) them |
| 6 | threaded only: `linker.define(..., "env", "memory", memory)`, then `instantiate_pre` | The shared memory. `instantiate_pre` resolves every import once, for all threads: nothing added later is seen |

A Linker refuses to define a name twice; `add_threads_to_linker` allows it
only around its replacements. Definitions QEMU does not import cost
nothing, so one Linker serves both builds and older ones; a missing one
fails instantiation with an error naming it. All imports are listed in
[reference/host-imports.md](reference/host-imports.md); adding one is in
[13-adding-features.md](13-adding-features.md).

### 4. The WASI context

- `inherit_stdio()`: QEMU's stdin, stdout and stderr are m-browser's.
- Arguments: the caller's list, `qemu-system-x86_64` (argv[0]) first. For
  the threaded build, if no argument is exactly `-smp`, `-smp N` is
  appended: `QEMU_SMP`, else 4 on hosts with at least 8 CPUs and 2 on
  smaller ones, clamped to 1..8 (log: `N vCPUs (QEMU_SMP)`). More vCPUs let
  Chromium's processes, X and the guest kernel run in parallel; the
  single-threaded build would only take turns, so it gets none.
- Environment: only variables whose names start with `QEMU_` or `WASI_`
  are copied in, so QEMU's knobs (`QEMU_JIT_HOT`, `WASI_DEBUG`, ...; see
  [reference/environment-variables.md](reference/environment-variables.md))
  reach it and the rest of the host's environment does not. m-browser's own
  `QEMU_*` variables are copied too, harmlessly.
- Preopens: each `(host, guest)` pair, with all permissions. The GUI and the
  headless runner preopen `pc-bios` as `/bios` and `.` as `/data`.

**Pitfall:** relative host paths are relative to the current directory.
Start m-browser from its repository directory (where `m.qcow2` and
`pc-bios/` are), or preopening fails or QEMU cannot find its disk.

### 5. Running

The display state (`QemuGfxState`, with the window's surface if there is
one) and the `JitCache` are created, then:

- **Single-threaded build.** A `QemuHostState` with its own WASI context
  (`WasiSlot::Own`) and `qemu_store_limits()`; a Store with the limiter and
  the profiler's hooks; `linker.instantiate`; the exports `memory` and
  `__indirect_function_table` are kept in the state for the JIT. If the
  instance exports `wasi_qemu_step`: `run_qemu_reactor`. Otherwise it is an
  old "command" build, whose `_start` returns only when QEMU exits (no
  migration possible).
- **Multi-threaded build.** `SharedMemory::new` with the import's type,
  `QemuThreads::new`, `add_threads_to_linker`, the main thread's Store from
  `QemuThreads::new_store`, `env.memory`, and `set_instance_pre` before any
  QEMU code runs (QEMU's C constructors, run by `_initialize`, already start
  a thread). Then `QemuThreads::instantiate` and `run_qemu_reactor` with
  `threaded = true` ([08-threads.md](08-threads.md)).

## QEMU's compiled-code cache (load_qemu_module)

QEMU's own code (interpreter, translator, device models) runs all session
long, so it is worth optimizing, unlike the TB modules.

1. Read the `.wasm` file; make a second engine, `speed_engine`, with the
   same settings and `OptLevel::Speed`.
2. Cache key: a hash of the wasm bytes and of
   `speed_engine.precompile_compatibility_hash()`, wasmtime's summary of
   the settings that affect compiled code.
3. If `qemu-<key>.cwasm` exists in the cache directory, load it into the
   main engine (`Module::deserialize_file`) and return. If that fails:
   `cached <path> unusable (<error>); recompiling`.
4. Otherwise compile with `speed_engine.precompile_module` (log:
   `compiling QEMU's module with optimizations (done once, then cached)...`,
   then `compiled in N s`), write the file through a temporary name and a
   rename (`store_compiled`: nobody sees half a file), delete all but the 3
   newest `qemu-*.cwasm` files (each is tens of megabytes), and load it.

A module compiled at `OptLevel::Speed` loads into the `OptLevel::None`
engine because wasmtime's compatibility check ignores the optimization
level, which does not change what the code does. Precompiling QEMU this
way made the emulator about 5% faster.

The directory is `$XDG_CACHE_HOME/m-browser/`, else `$HOME/.cache/m-browser/`,
else `m-browser/` in the system's temporary directory. To clear it:

```sh
rm -f "${XDG_CACHE_HOME:-$HOME/.cache}"/m-browser/qemu-*.cwasm   # any directory
```

| `QEMU_WASM_OPT` | Effect |
|---|---|
| unset | As above |
| `none` | No cache: QEMU is compiled unoptimized at every start, as before the cache existed |
| `speed` | The engine itself optimizes: TB modules too (slower compiles; an experiment). QEMU still comes from the cache |

**Note:** the key uses Rust's `DefaultHasher`, which may change between
Rust releases (one recompile after a toolchain update). Settings that
change the code, such as `QEMU_PROFILE`'s epoch checks, get their own file.

**Pitfall:** loading a `.cwasm` runs its machine code without any checks
(hence `unsafe`): whoever can write the cache directory can make m-browser
run anything. Keep it private.

## Driving QEMU (run_qemu_reactor)

QEMU is linked as a WASI reactor (patch 0007 in qemu-wasi): m-browser calls
its exports.

1. `_initialize`: wasi-libc's start-up code; runs QEMU's C constructors.
2. `wasi_qemu_init()`: read the arguments, start the machine
   (`qemu_init`); returns 0.
3. `wasi_qemu_step()`, in a loop: one iteration of QEMU's main loop
   (timers, devices, display, network; in the single-threaded build also
   about 30 ms of guest execution). Returns -1 to go on, or the exit status.

Between two steps no QEMU code is running (no wasm frames are live). That
is where it is safe to stop QEMU, print statistics and move it to a new
Store.

| Outcome | Log | Result |
|---|---|---|
| `exit(n)` during `wasi_qemu_init` (bad option, missing disk image) | `QEMU exited during startup with status n (its error message is above)` | error |
| `wasi_qemu_init` returns n != 0 (it could not read its arguments) | `wasi_qemu_init returned n` | error |
| trap during `wasi_qemu_init` | `QEMU init failed` and the trap | error |
| a step returns n >= 0 (guest powered off, `quit` in the monitor, after `wasi_qemu_quit`), or `exit(n)` in a step | `[qemu] exited with status n` | `Ok` |
| trap in a step | `[qemu] [  t s] step failed: ...` with the wasm backtrace | error `QEMU execution failed` |

**Stopping.** The caller sets `stop`: the GUI when the QEMU page is closed
or replaced or the browser quits, the headless runner after
`QEMU_QUIT_AFTER`. Before the next step, `wasi_qemu_quit` is called (log:
`asking QEMU to quit`). It requests a shutdown, as closing QEMU's window
would: over the following steps the guest is powered off and QEMU flushes
and closes its disk images, until a step returns the exit status. After
`QUIT_TIMEOUT` (10 s) without that, the log says
`QEMU did not quit within 10s; stopping it` and the function returns. A
build without `wasi_qemu_quit` is simply dropped.

**Note:** for the guest, `wasi_qemu_quit` is a power cut: its file systems
are not unmounted, although the qcow2 file stays consistent. For a clean
guest, shut it down from inside (`poweroff`); QEMU then exits by itself.

**Pitfall:** when the browser quits, `App::shutdown_qemu` waits only 5 s
for QEMU's thread, less than `QUIT_TIMEOUT`, then the process ends anyway.

## Store migration (migrate_store)

**Why.** wasmtime frees an instance only when its Store is dropped. Every
TB module QEMU loads becomes an instance in QEMU's Store, hundreds of
thousands over a session. Each instance keeps its compiled module alive
with that module's executable memory mappings (about two per module), even
after the module has left the JIT's module cache. Past `vm.max_map_count`
the next mapping fails and the process aborts. So between steps the runner
moves QEMU into a fresh Store and drops the old one.

**When.** After each step: if `instances_in_store` (TB instances in this
Store) has reached `migrate_after_instances()`; or, every
`MAP_CHECK_INTERVAL` new instances, if `process_map_count()` (the lines of
`/proc/self/maps`; without that file only the instance count applies) is
above `MAP_COUNT_SOFT_LIMIT`.

| Constant | Value | Role |
|---|---|---|
| `MIGRATE_AFTER_INSTANCES` | 100,000 | Instances per Store before a move. `QEMU_MIGRATE_AFTER=<n>` overrides it (for testing: moves are rare otherwise). Each instance holds about 0.4 KB |
| `MAP_CHECK_INTERVAL` | 512 | New instances between two counts of the mappings |
| `MAP_COUNT_SOFT_LIMIT` | 45,000 | Mapping count above which QEMU moves. One step creates at most about 1,000 instances (~2,000 mappings), well within the headroom |
| `qemu_store_limits()` (qemu_jit.rs) | 150,000 | wasmtime's per-Store instance cap, raised from its default of 10,000; above 100,000 with room for one more step |
| `MODULE_CACHE_MAX` (qemu_jit.rs) | 18,000 | Compiled TB modules kept for reuse |

After a move, if the mappings still exceed two thirds of the soft limit
(30,000), the module cache alone holds most of the budget and moves would
follow each other: the cache is halved (log:
`[qemu-jit] module cache trimmed to K modules; M memory mappings`).

History: the first version moved every 7,000 instances, below wasmtime's
default cap. Moving less often needed the higher cap, and an attempt to
move at 50,000 instances without watching the mapping count crashed with
ENOMEM once the mappings passed `vm.max_map_count`. Since then the mapping
count decides.

**How.** `migrate_store(store, instance, linker, module)`:

1. Take the old instance's `memory` and the module's initial memory size
   (256 pages, 16 MiB, for current builds).
2. Snapshot it with `nonzero_runs(data, initial)`: everything below the
   initial size, plus each 64 KiB chunk above it that is not all zeros, as
   `(offset, bytes)` runs, neighbouring chunks merged.
3. `store.into_data()`: drop the old Store, which is what frees the TB
   instances, and keep its data (WASI files, display state, JIT cache).
4. Reset the per-Store parts of the data: `release_wasi`,
   `jit.reset_for_new_store()`, `guest_memory`, `guest_table`,
   `pending_exception`.
5. Make a new Store with the data, the limiter and the profiler's hooks,
   and instantiate the same Module with the same Linker.
6. Grow the new memory to the old size and copy the runs into it; record
   the new memory and table in the data.
7. Call `wasi_qemu_jit_reset`: QEMU forgets its TB handles, which indexed
   the old function table, and loads or interprets those TBs again.
8. Log `[qemu] moved QEMU to a fresh store: C of T MiB copied, N TB modules
   freed, X ms`, then the JIT statistics.

Why copying the memory is enough: QEMU's state is in its linear memory, and
its only mutable global, `__stack_pointer`, is back at its initial value
between steps. Below the initial size lie the data segments, which
instantiation has just reset, so that part is copied in full; memory above
it starts out zeroed, so zero chunks need no copy (unused guest RAM is
skipped, which saves time and resident memory). Function pointers in memory
are table indices, and QEMU's own table entries are the same in every
instance; only the TB entries are gone, hence step 7. Host state is in the
Store's data or the Linker's closures and carries over. `_initialize` is
not run again: its effects are in the copied memory.

A move pauses QEMU (copying the memory took about 0.5 s) and every TB in
use is instantiated again, so moves should stay rare.

**Pitfall:** call `migrate_store` only between steps, never from a host
function: QEMU's frames would still be using the old Store.

**Pitfall:** anything QEMU's instance keeps outside its linear memory (a
new mutable global, a table entry, state behind an import) is lost by a
move. Keep state in memory or in `QemuHostState`, or restore it in
`migrate_store`.

The multi-threaded build never calls `migrate_store`: its main thread runs
no TBs, and its vCPU threads replace their own Stores without copying
anything ([08-threads.md](08-threads.md)).

## JIT statistics (report_jit_stats)

Printed after each move, and every minute (checked after each step, in
both builds) with `QEMU_JIT_STATS` set. The first line is one line in the
log:

```text
[qemu-jit] compiled A TBs in B s total, C in the background (D s of other
threads), E reused from cache, F loads shared an instance; G instances in
the store; H memory mappings
[qemu-jit] compile cost by size: <512B: n x t ms avg b B, <1K: ...
```

| Field | Source | Meaning |
|---|---|---|
| A, B | `JitCache::stats_compiled`, `stats_compile_time` | Modules compiled synchronously, on QEMU's thread (`wasi_jit_compile`). By default only TBs the interpreter cannot run; every hot TB with `QEMU_JIT_ASYNC=0` or `QEMU_JIT_HOT=0` |
| C, D | `background_compiled()`, `background_compile_time()` | Modules compiled by the `qemu-jit-N` threads, rejected ones included; D is those threads' time, not time QEMU waited |
| E | `stats_cache_hits` | Loads that found the module compiled already and only instantiated it (after a move or a TB buffer flush, or on another vCPU) |
| F | `QemuJitState::stats_shared` | Loads answered by an instance already in this Store: no compile, no new instance. Counts since QEMU started |
| G | `instances_in_store` | TB instances in the current Store: what the next move frees |
| H | `process_map_count()` | The process's memory mappings (`?` if unknown); compare with 45,000 and `vm.max_map_count` |

The second line, with `QEMU_JIT_STATS` only, gives count, average compile
time and average size per module size; empty sizes are left out. It counts
synchronous compiles only, so with default settings it is usually empty.

**Note:** in the multi-threaded build the report comes from the main
thread's Store, where no TB runs: F and G are always 0 there. A to E come
from the shared cache and cover all vCPUs.

## The component runtime (Wasm)

The top of `src/wasm.rs` runs wasi-gfx components and shares nothing with
QEMU's path. `Wasm::new(rx)` builds an engine (component model, async,
epoch interruption), a Linker with wasi-webgpu, wasi-graphics-context,
wasi-surface and WASI preview 2, and a Store with epoch deadline 1. A
thread polls `rx` every 100 ms and on a message advances the epoch, which
makes the running component trap: that is how it is stopped. `run_wasm`
hands over the window's surface (`create_canvas` takes it), instantiates
the component as a `wasi:cli` command and calls `run`; `stop_wasm` does
nothing. See [05-browser-ui.md](05-browser-ui.md).

## Troubleshooting

| Symptom | Likely cause | What to do |
|---|---|---|
| `QEMU exited during startup with status 1 (its error message is above)` | QEMU rejected an option or could not open a file (disk image, firmware) | Read QEMU's message just above (`qemu-system-x86_64: ...`). Run m-browser from the directory holding `m.qcow2` and `pc-bios/` |
| `Failed to load module`, `failed to load wasm module` | Wrong path, or not a wasm file | Check the path; `wasm-tools validate qemu-system-x86_64.wasm` |
| Instantiation error naming an import | QEMU and m-browser from different versions of the interface | Install a QEMU build matching this m-browser; compare `wasm-tools print --skeleton` with [reference/host-imports.md](reference/host-imports.md) |
| A pause at start with `compiling QEMU's module with optimizations` | First start after a new QEMU build, or a wasmtime or Rust update | Wait; later starts load the cache |
| It recompiles at every start | `cannot cache the compiled module: ...` (directory not writable), or `cached ... unusable` each time | Fix the directory's permissions or set `XDG_CACHE_HOME`; delete the cached files |
| Abort with ENOMEM while RAM is free; H near 65,000 | Too many memory mappings | Look for `moved QEMU to a fresh store` lines; lower `QEMU_MIGRATE_AFTER`; or raise the limit: `sudo sysctl -w vm.max_map_count=262144` |
| `[qemu-jit] compile: instantiation failed: ...` about instances | More than 150,000 instances in one Store, e.g. `QEMU_MIGRATE_AFTER` set above that | Keep `QEMU_MIGRATE_AFTER` well below 150,000, or raise `qemu_store_limits()` with it |
| `moved QEMU to a fresh store` every few seconds | Heavy TB churn: many TB buffer flushes (`[qemu-wasi] TB buffer flush #N`) | Keep `-accel tcg,tb-size=256`; see [12-profiling.md](12-profiling.md) |
| `QEMU did not quit within 10s; stopping it` | Shutdown stuck | The disk image may not be closed cleanly; check the guest's file systems at the next boot |
| `step failed:` with a wasm backtrace | QEMU trapped (assertion, `abort()`, bad memory access) | See [14-testing-and-debugging.md](14-testing-and-debugging.md) |
