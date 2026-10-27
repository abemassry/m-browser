# The multi-threaded QEMU (src/qemu_threads.rs)

**What this covers.** How m-browser runs the multi-threaded build of QEMU
(`make THREADS=1` in qemu-wasi), which gives every virtual CPU an OS thread
of its own: how threads are created, how each virtual CPU runs in a Store
that can be replaced, what the threads share and how access to it is
serialized, and what happens when a thread fails. Read it before changing
`src/qemu_threads.rs` or the `threaded` paths of `src/wasm.rs`, or when the
threaded build hangs, crashes or is slow. QEMU's side is described in the
qemu-wasi repository's docs/08-threads.md.

## Why

The single-threaded build runs the guest's only virtual CPU inside QEMU's
main loop, in time slices: while the guest is busy, everything takes
turns. The threaded build runs several virtual CPUs at once, so Chromium's
processes, X and the guest kernel progress in parallel. Measured on the
development machine:

| | Threaded, 4 vCPUs | Single-threaded |
|---|---|---|
| Alpine desktop appears | about 2:35 | about 2:03 |
| Chromium's window appears | about 3:55 | about 3:40 |
| Clicks while Chromium starts | handled in 6 to 9 s | lost |

Booting is a little slower; staying responsive under load is the gain.
This is the build installed in m-browser.

## Words used in this document

| Term | Meaning |
|---|---|
| vCPU | A virtual CPU of the guest; `-smp N` gives the guest N of them |
| MTTCG | QEMU's multi-threaded TCG: one host thread per vCPU, each translating and running guest code |
| wasi-threads | The WebAssembly convention for threads: each thread is a separate *instance* of the same module, all sharing one memory. A thread is requested through the `wasi.thread-spawn` import and starts in the `wasi_thread_start` export |
| Store, instance | wasmtime's container for running code (freed only as a whole), and a running copy of a module ([06-qemu-runner.md](06-qemu-runner.md)) |
| shared memory | A linear memory used by several instances at once (`wasmtime::SharedMemory`) |
| BQL | QEMU's "big QEMU lock", a global mutex around device emulation and the main loop |
| TLS | Thread-local storage: C `__thread` variables. In wasm, a per-thread block in linear memory whose address is in the `__tls_base` global |
| shadow stack | The stack in linear memory that C uses for locals whose address is taken; the `__stack_pointer` global points into it. It is separate from the native stack, where wasmtime runs the machine code |
| spin lock | A lock whose waiters loop instead of sleeping |
| call hook | A wasmtime callback run on every transition between wasm and host code |

## Architecture

```text
m-browser process
+--------------------------------------------------------------------------+
| shared by every QEMU thread:                                             |
|   QEMU's memory: one SharedMemory, imported by every instance            |
|   SharedWasi: one WASI context (open files) behind a spin lock           |
|   JitCache: compiled TB modules   QemuGfxState: display   VirtualNet     |
+--------------------------------------------------------------------------+
OS thread:  runner thread    qemu-1             qemu-2              qemu-3 ...
Store:      main             T1                 T2                  T3
runs:       QEMU's main      call_rcu           vCPU 0's thread:    vCPU 1 ...
            loop (never      (typically)        mttcg_cpu_thread_fn
            replaced)                           -> env.host_vcpu_run
                                                   Store V0: vCPU 0's
                                                   loop and TB instances
                                                   (replaced now and then)
other threads: qemu-jit-0..3 (background compiler), qemu-net (network)
```

Every QEMU thread has its own Store and instance on its own OS thread, all
built from one `InstancePre` (the module with its imports resolved). The
runner thread is the one that called `run_wasm_module`: an unnamed thread
of the GUI, or the headless runner's main thread.

## Recognizing the build and choosing the vCPU count

`run_wasm_module` treats a module that imports `env.memory` as the threaded
build. If QEMU's arguments contain no `-smp`, it appends `-smp N`:

| Condition | N |
|---|---|
| `QEMU_SMP=<n>` set | n, clamped to 1..8 (unparsable: the default) |
| host has 8 or more CPUs | 4 |
| otherwise | 2 |

**Pitfall:** QEMU keeps one JIT handle per vCPU in each TB, 8 slots
(`WASI_JIT_SLOTS`): an explicit `-smp 9` or more makes QEMU abort with
`at most 8 vCPUs are supported`. The check looks for the exact argument
`-smp`; a spelling such as `--smp` is not seen.

## Start-up

`run_wasm_module`, threaded branch ([06-qemu-runner.md](06-qemu-runner.md)):

1. Create the memory with `SharedMemory::new`, using the import's type
   (256 pages, 16 MiB, to start; at most 65,536 pages, 4 GiB).
2. `QemuThreads::new(engine, memory, wasi_ctx, gfx, jit_cache)` wraps the
   WASI context in a `SharedWasi` and warns on hosts that are not x86-64.
3. `add_threads_to_linker`, after WASI's functions (it replaces some).
4. The main thread's Store (`QemuThreads::new_store`), `env.memory` in the
   Linker, then `set_instance_pre(linker.instantiate_pre(&module))`. This
   must happen before any QEMU code runs: `_initialize` runs QEMU's C
   constructors, and RCU's constructor already creates a thread.
5. `QemuThreads::instantiate` and `run_qemu_reactor(..., threaded = true)`:
   the main thread runs the main loop, its Store never moves.

## Creating a thread

1. QEMU calls `pthread_create()`. wasi-libc allocates the thread's shadow
   stack (QEMU asks for 16 MiB, in util/qemu-thread-posix.c) and TLS
   block, and calls the import `wasi.thread-spawn(start_arg)`.
2. `QemuThreads::spawn` takes the next thread id (1, 2, ...) and starts an
   OS thread named `qemu-<id>` with a 16 MiB native stack (`THREAD_STACK`).
   It returns the id at once, or -6 (EAGAIN) if the OS refuses the thread.
3. On the new thread: `new_store()` makes a Store whose data shares the
   WASI context, display, JIT cache and memory, with the call hook of
   `install_store_hooks` and `qemu_store_limits()`.
4. `instantiate()` makes an instance from the `InstancePre`. The module's
   start function, `__wasm_init_memory` (generated by the linker), only
   initializes memory for the first instance; later ones skip it, so the
   running QEMU's data is not reset.
5. It calls `wasi_thread_start(id, start_arg)`. wasi-libc's code sets the
   instance's stack pointer and TLS base from `start_arg` and runs the
   thread's function. When that returns, the OS thread ends; an error goes
   to `thread_failed`.

## How a vCPU runs: host_vcpu_run and wasi_vcpu_enter

A Store frees its TB instances only when dropped, and can only be dropped
when none of its code is running. A vCPU thread's own instance is inside
`mttcg_cpu_thread_fn` for the thread's whole life, so its Store can never
be dropped. QEMU (patch 0008) therefore does not run the vCPU loop in that
instance:

1. `wasi_vcpu_thread_run` (accel/tcg/tcg-accel-ops-mttcg.c) computes
   `stack_top`, 64 bytes below one of its locals and 16-byte aligned (so
   below its own frame on the shadow stack), takes its TLS base, and calls
   `env.host_vcpu_run(cpu_index, stack_top, tls_base)`.
2. `QemuThreads::run_vcpu`, on the same OS thread, makes a separate vCPU
   Store with a new instance of QEMU (sharing the memory) and calls its
   export `wasi_vcpu_enter(stack_top, tls_base, flags)` in a loop.
3. `wasi_vcpu_enter`, a few lines of assembly in tcg/wasm32.c, sets the new
   instance's `__stack_pointer` and `__tls_base` globals and calls
   `wasi_vcpu_step(flags)`: one iteration of the vCPU loop (run guest code
   until something needs attention, then handle pending events; it may
   wait there while the vCPU is halted). It returns 0 to go on, non-zero
   once the vCPU is unplugged.
4. Between two calls none of the vCPU Store's code is running, so
   `run_vcpu` can drop that Store and make another.

Why the two addresses: a fresh instance's globals hold the module's
initial values, the main thread's stack pointer and a TLS base of 0.
Running with them would overwrite the main thread's stack and read the
wrong thread-local variables (`current_cpu`, the TCG context, the TB handle
list...). The linker only lets assembly set these globals; wasi-libc's
`wasi_thread_start` does the same. The OS thread stays the same because
those thread-local variables, and QEMU's idea of which thread this is,
belong to it.

```text
vCPU thread qemu-2: one OS thread, two stacks
native stack (16 MiB, THREAD_STACK)       shadow stack in QEMU's memory
newest frame at the bottom                 (16 MiB, from pthread_create)
+--------------------------------------+   +-------------------------------+
| wasi_thread_start ...      Store T2  |   | mttcg_cpu_thread_fn           |
| mttcg_cpu_thread_fn                  |   | wasi_vcpu_thread_run          |
| wasi_vcpu_thread_run                 |   |   stack_top: just below it    |
| env.host_vcpu_run -> run_vcpu (Rust) |   | wasi_vcpu_step and callees    |
| wasi_vcpu_enter            Store V0  |   |   (vCPU Store's instance)     |
| wasi_vcpu_step, TBs, helpers         |   +-------------------------------+
+--------------------------------------+   TLS block: tls_base, shared by
                                           both instances
```

## Replacing a vCPU's Store (run_vcpu)

```text
loop, one pass per vCPU Store:
    store = new_store(); enter = instantiate(store).wasi_vcpu_enter
    flags = VCPU_JIT_RESET
    loop:
        r = enter(stack_top, tls_base, flags); release_wasi(); flags = 0
        error -> thread_failed;  r != 0 (unplugged) -> return
        every 512 new instances: mappings > 45,000 -> migrate_epoch += 1
        instances >= migrate_after_instances() or migrate_epoch changed
            -> leave the inner loop
    drop(store)    # frees this vCPU's TB instances
    log "[qemu] vCPU N: moved to a fresh store, K TB modules freed"
    mappings > 30,000 -> halve the module cache
```

- `VCPU_JIT_RESET` (1), passed on the first call into each new Store, makes
  QEMU forget this vCPU's TB handles (`wasi_tb_cache_forget_all`): they
  indexed the old Store's function table.
- Memory mappings belong to the whole process, and one vCPU cannot free
  another's instances, so a vCPU that sees too many bumps `migrate_epoch`
  and every vCPU moves at its next return.
- Unlike `migrate_store` in the single-threaded build, nothing is copied
  (the memory is shared) and the other threads keep running. The limits
  are the same constants (`MIGRATE_AFTER_INSTANCES`, `MAP_CHECK_INTERVAL`,
  `MAP_COUNT_SOFT_LIMIT`, `QEMU_MIGRATE_AFTER`).
- The checks run only when `wasi_vcpu_enter` returns; an idle vCPU may
  stay inside for long.

## The shared WASI context (SharedWasi)

All threads must see the same open files (the disk image, for one). But
wasmtime-wasi's functions take a `&mut WasiP1Ctx` from the calling Store's
data, which cannot be shared between Stores without a lock:

1. Every Store's data holds `WasiSlot::Shared(Arc<SharedWasi>)`.
2. A wasmtime-wasi function gets the context through
   `QemuHostState::wasi_ctx()`, which takes the lock (unless this Store
   holds it already) and sets `wasi_locked`.
3. When the function returns to wasm, the call hook installed by
   `install_store_hooks` (on `CallHook::ReturningFromHost`) calls
   `release_wasi()`, which unlocks.
4. `spawn` and `run_vcpu` also call `release_wasi()` after their calls
   return, in case a call ended in an error.

`SharedWasi::lock` is a spin lock: 100 spins, then `yield_now()` in a loop.
File operations are short, so this is cheaper than sleeping. As a result,
the WASI calls of all QEMU threads run one at a time, and a long one (an
`fd_sync` of the disk image, a large write, a blocking read) holds up
every other thread's file I/O; the waiters keep yielding, which shows as
CPU use.

**Pitfall:** never take the WASI context (`wasi_ctx()`) in a host function
that also holds another lock or waits for another QEMU thread: that thread
may be spinning for the WASI lock. And never run another Store's wasm
from inside a WASI call: the lock is not re-entrant across Stores, even on
the same OS thread.

**Pitfall:** every Store with a shared context needs `install_store_hooks`
(`new_store` does it), or its first WASI call locks forever. A Store has a
single call hook: setting another one, such as the profiler's
(`qemu_profile::attach`), replaces it. Combine them in one closure if both
are ever needed. The hook needs wasmtime's `call-hook` Cargo feature.

## WASI functions replaced in the threaded build

`add_threads_to_linker` redefines these (with `allow_shadowing(true)`), so
that frequent calls never take the WASI lock:

| Function | Replacement | Why |
|---|---|---|
| `clock_time_get` | `clock_now`: clock 0 (realtime) from `SystemTime`; clocks 1 to 3 (monotonic and CPU time) from one process-wide `Instant`, plus 1 s | vCPUs read the clock all the time (guest `rdtsc`, timers). One origin for all threads |
| `clock_res_get` | 1 ns for clocks 0 to 3 | Completeness; current builds do not import it |
| `random_get` | `rand::rng()` fills the buffer | No context needed |
| `sched_yield` | `std::thread::yield_now()` | No context needed |
| `poll_oneoff` | Clock subscriptions only: sleep until the earliest deadline, report that one event | This is how wasi-libc sleeps (`nanosleep`, `usleep`); wasmtime-wasi's would hold the WASI lock for the whole sleep |

Errors: an unknown clock gives `EINVAL` (28), a bad pointer `EFAULT` (21),
and a `poll_oneoff` on file descriptors `ENOTSUP` (58) with the warning
`poll_oneoff on file descriptors is not supported` (QEMU polls none).
QEMU's mutexes and condition variables (wasi-libc's pthreads) use wasm's
atomic wait and notify instructions, which wasmtime implements itself:
they never reach m-browser.

## Other shared state

- **JIT cache.** One `JitCache` and background compiler for all vCPUs: a
  TB compiled for one vCPU is a cache hit for the others, which only
  instantiate it in their own Store ([07-jit-host.md](07-jit-host.md)).
- **Display.** One `Arc<Mutex<QemuGfxState>>`. QEMU refreshes the display
  and polls input on its main thread.
- **Network.** The `VirtualNet` is captured by the Linker's closures, so
  every Store uses it; any thread may send (a vCPU writing to the network
  card). The main thread sleeps in `host_net_wait` with the BQL released;
  when another thread needs the main loop (`aio_notify` calls
  `wasi_main_wake`), QEMU calls `env.host_main_wake`, which ends that sleep
  (`VirtualNet::wake_main`, [10-networking.md](10-networking.md)).
  Without a user network (`-nic none`) QEMU sleeps with `nanosleep`, which
  nothing can interrupt, and the main loop reacts up to 30 ms late.
- **The main thread's Store** never moves: no TB runs on the main thread.

**Note:** `QemuThreads` holds the `InstancePre`, which holds the Linker's
host functions, whose closures hold `QemuThreads`: a reference cycle. As
far as the code shows (this was not measured), the threaded QEMU's shared
memory, JIT cache, compiler threads and network, port forwards included,
are therefore only freed when m-browser exits, not when QEMU does.

## When a thread fails

wasi-threads' rule is that a trap or `exit()` in any thread ends the whole
program; QEMU cannot go on with a vCPU missing anyway. `thread_failed`
implements it:

| Event | Log | Result |
|---|---|---|
| A QEMU thread or vCPU Store traps (assertion, `abort()`, bad access) | `[qemu] thread N failed: ...` or `[qemu] vCPU N failed: ...` | The process exits with status 1 |
| `exit(n)` on such a thread | `[qemu] QEMU exited with status n (vCPU N)`, or `(thread N)` | The process exits with status n |
| The OS refuses a thread | `[qemu] cannot start a thread: ...` | `thread-spawn` returns -6; QEMU treats that as fatal |
| A trap or `exit()` on the main thread | as in [06-qemu-runner.md](06-qemu-runner.md) | `run_wasm_module` returns |
| A Rust panic in a host function on a QEMU thread | `thread 'qemu-N' panicked at ...` | Only that OS thread ends; QEMU probably hangs |

**Pitfall:** in the GUI, `thread_failed` ends the whole browser, and the
disk image is not closed (as if the process were killed).

**Note:** a normal end of QEMU unplugs every vCPU (in `wasi_qemu_step`),
which is meant to make their `run_vcpu` and threads return. From reading
the code, they do not: `wasi_qemu_step` returns with the main thread still
holding the BQL, and a vCPU thread must take the BQL to see the unplug, so
it waits forever ([known-issues.md](known-issues.md); the fix belongs in
QEMU's `system/main.c`, see the qemu-wasi repository's
docs/known-issues.md). If the main thread traps, calls `exit()`, or does
not quit within `QUIT_TIMEOUT`, nothing unplugs them at all. Either way
the threads stay until m-browser exits.

## Memory ordering: x86-64 hosts only

x86 CPUs make one CPU's memory accesses visible to the others in program
order, nearly ("total store order"). QEMU normally adds barriers when the
guest's ordering is stronger than the host's; the wasm backend declares
them unneeded for x86 guests (patch 0008), because TB code keeps accesses
in program order and x86-64 hosts keep that order too. wasmtime compiles
plain wasm loads and stores to plain host instructions, so on an ARM host
another vCPU may see them out of order, and guest code relying on x86
ordering (lock-free kernel code, say) may break. `QemuThreads::new` warns:
`on this host, the vCPU threads may see each other's memory accesses out
of order`.

## Profiling

`QEMU_PROFILE` does not work with this build: it prints `QEMU_PROFILE does
not follow the multi-threaded QEMU's vCPU threads` and starts no profiler
(the vCPUs run in Stores the profiler never sees). QEMU's own
`QEMU_PROF_EVERY` report is also single-threaded only. Profile with the
single-threaded build ([12-profiling.md](12-profiling.md)).

## Debugging tips

| Thread name | What it is |
|---|---|
| `m-browser` | The GUI thread, and QEMU's main thread (unnamed, so it shows the process name) |
| `qemu-<id>` | QEMU threads, in creation order: usually `qemu-1` is RCU's `call_rcu`, then one per vCPU |
| `qemu-jit-<n>` | Background compiler threads |
| `qemu-net` | The network thread |

```sh
# any directory; the newest m-browser process
ps -L -o tid,comm,pcpu,time -p "$(pgrep -n m-browser)"   # CPU time per thread
top -H -p "$(pgrep -n m-browser)"                        # live, per thread
```

- A busy guest shows its vCPU threads near 100% each; an idle one near 0
  (a halted vCPU waits in QEMU without using CPU). `qemu-jit-*` are busy in
  bursts (boot, starting programs).
- Several threads using CPU while the guest is idle suggests WASI lock
  contention (a long file operation) or a guest spinning.
- `gdb -p <pid> -batch -ex 'thread apply all bt'` (may need sudo) shows
  where each thread waits in Rust code (`SharedWasi::lock`,
  `wait_for_frame`, wasmtime's atomic wait); wasm frames appear as bare
  addresses.
- QEMU's monitor (Ctrl+Alt+2): `info cpus` lists the vCPUs, `info
  registers -a` shows all their registers.
- Reproduce with `QEMU_SMP=1`, then with the single-threaded build
  (`make install` in qemu-wasi; `make THREADS=1 install` goes back), to
  tell threading bugs from others.
