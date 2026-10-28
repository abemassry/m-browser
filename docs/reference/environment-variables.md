# Environment variables

**What this covers.** Every environment variable m-browser reads: for
running QEMU (vCPUs, JIT, Store migration, the compiled-module cache), its
network, its profiler and its headless runner; the standard variables that
matter; and the rule that passes `QEMU_*` and `WASI_*` variables into
QEMU. For each: default, accepted values, effect and where the code reads
it. The variables QEMU itself reads (JIT tiers, time slices, diagnostics,
display) are documented in the qemu-wasi repository's
docs/reference/environment-variables.md.

## How to set variables

Put them on m-browser's command line. Run m-browser from its own
directory: the GUI finds `pc-bios/` and the disk image `m.qcow2` there.

```sh
# from the m-browser directory
QEMU_SMP=2 QEMU_NET_DEBUG=1 ./target/release/m-browser
QEMU_SNAPSHOT=/tmp/screen.png QEMU_QUIT_AFTER=300 \
./target/release/m-browser --qemu-headless qemu-system-x86_64.wasm \
    -L /bios -drive file=/data/m.qcow2,format=qcow2 -m 1024
```

- The environment is fixed when m-browser starts: restart it to change a
  value.
- Logs and reports go to stderr (a few progress messages to stdout), so
  start m-browser from a terminal.
- A number that does not parse gives the default, without a warning. Only
  the headless runner's `QEMU_TYPE` and `QEMU_QUIT_AFTER` stop with an
  error instead.
- *Presence* in the Values column means that any value, even `0` or an
  empty one, switches the variable on.
- Paths in m-browser's variables are ordinary host paths, relative to
  m-browser's working directory. Paths in QEMU's own variables are inside
  QEMU's sandbox.
- Every variable whose name starts with `QEMU_` or `WASI_` also goes into
  QEMU, m-browser's own included ([below](#variables-passed-into-qemu)).
- Some variables only matter for one QEMU build: the *single-threaded*
  one (qemu-wasi's `make`) or the *multi-threaded* one, a thread per vCPU
  (`make THREADS=1`). m-browser tells them apart by whether the module
  imports its memory ([08-threads.md](../08-threads.md)).

## Running QEMU and its JIT

| Variable | Default | Values | Effect | Read in | More |
|---|---|---|---|---|---|
| `QEMU_SMP` | 4 on hosts with at least 8 CPUs, else 2 | Integer, clamped to 1..8 (8 is the most this QEMU supports) | Number of vCPUs for the multi-threaded QEMU: m-browser appends `-smp N` to QEMU's arguments, unless one of them is exactly `-smp`. Logged as `N vCPUs (QEMU_SMP)`. Not used for the single-threaded QEMU, which gets no `-smp`. `QEMU_SMP=1` helps to rule out threading bugs | `src/wasm.rs`, `run_wasm_module()` | [08-threads](../08-threads.md), [06-qemu-runner](../06-qemu-runner.md) |
| `QEMU_WASM_OPT` | unset | `none` or `speed`, exactly; anything else: as unset | Unset: TB modules (the small modules QEMU makes from guest code) are compiled unoptimized, for speed of compiling, and QEMU's own module is compiled optimized once and cached ([below](#cache-and-temporary-files)). `none`: QEMU's module too is compiled unoptimized, at every start, without the cache. `speed`: everything is optimized, TB modules included (slower compiles; an experiment) | `src/wasm.rs`, `run_wasm_module()` (`speed`), `load_qemu_module()` (`none`) | [06-qemu-runner](../06-qemu-runner.md) |
| `QEMU_JIT_THREADS` | A quarter of the host's CPUs, 1 to 4 | Integer; 0 gives 1; no upper limit when set | Threads that compile TB modules in the background. They start with QEMU's first background compile request, so they are unused when QEMU compiles synchronously (QEMU's `QEMU_JIT_ASYNC=0` or `QEMU_JIT_HOT=0`). Logged as `compiling TBs in the background on N threads` | `src/qemu_jit.rs`, `BackgroundCompiler::start()` | [07-jit-host](../07-jit-host.md) |
| `QEMU_JIT_STATS` | off | Presence | Every 60 s, prints `[qemu-jit] compiled N TBs in T s total, ...` (TBs compiled and time spent, background share, cache reuse, instances in the Store, the process's memory mappings), followed by `[qemu-jit] compile cost by size:` (count, mean compile time and mean size per module size class). The single-threaded QEMU's Store migrations print the first line anyway. With the multi-threaded QEMU, the Store numbers are the main thread's, which runs no TBs | `src/wasm.rs`, `run_qemu_reactor()`, `report_jit_stats()` | [06-qemu-runner](../06-qemu-runner.md), [07-jit-host](../07-jit-host.md) |
| `QEMU_JIT_DUMP` | off | `<dir>` or `<dir>,<n>`; `n` defaults to 100 (also when it does not parse), 0 gives 1 | Saves every n-th TB module compiled, in the foreground or in the background, as `<dir>/tb-NNNNNNN.wasm` (the compile count). The directory is created; if that fails, nothing is saved and nothing is said. Read the files with `wasm-tools print`. Not the same as QEMU's `QEMU_TB_DUMP`, which saves one chosen TB | `src/qemu_jit.rs`, `dump_tb_module()`, from `JitCache::get_or_compile()` and `JitCache::collect_background()` | [07-jit-host](../07-jit-host.md) |
| `QEMU_MIGRATE_AFTER` | 100000 | Integer (`u32`); 0 means after every step | A wasmtime *Store* holds module instances and frees them only all at once, so after this many TB module instances a Store is replaced by a fresh one. Single-threaded QEMU: QEMU moves to a new Store and its memory is copied, a pause of about half a second. Multi-threaded: each vCPU thread replaces its own Store. A move also happens when the process has more than 45,000 memory mappings, whatever this says. Mostly for testing moves, which are rare otherwise | `src/wasm.rs`, `migrate_after_instances()`, used by `run_qemu_reactor()` and by `run_vcpu()` in `src/qemu_threads.rs` | [06-qemu-runner](../06-qemu-runner.md), [08-threads](../08-threads.md) |

## Cache and temporary files

| Variable | Default | Values | Effect | Read in | More |
|---|---|---|---|---|---|
| `XDG_CACHE_HOME`, `HOME` | Cache in `~/.cache/m-browser/` | Directories | Where QEMU's optimized, compiled module is cached: `$XDG_CACHE_HOME/m-browser/`, else `$HOME/.cache/m-browser/`, else `m-browser/` in the temporary directory. Files are `qemu-<hash>.cwasm`; the 3 newest are kept. An empty `XDG_CACHE_HOME` counts as set, which puts the cache in `./m-browser/`. Not used with `QEMU_WASM_OPT=none` | `src/wasm.rs`, `load_qemu_module()` | [06-qemu-runner](../06-qemu-runner.md) |
| `TMPDIR` | `/tmp` | Directory | The temporary directory (Rust's `std::env::temp_dir()`): the cache's last fallback, and where a TB module that fails a synchronous compile is saved, as `qemu-jit-failed.wasm` | `src/wasm.rs`, `load_qemu_module()`; `src/qemu_jit.rs`, `jit_load()` | [07-jit-host](../07-jit-host.md) |

## Network

| Variable | Default | Values | Effect | Read in | More |
|---|---|---|---|---|---|
| `QEMU_NIC` | `user` | A value for QEMU's `-nic` option | Replaces the value of the `-nic` option the GUI starts QEMU with. For example `user,hostfwd=tcp:127.0.0.1:2222-:22` forwards host port 2222 to the guest's port 22; forwarding works for TCP only, and without a host address the port listens on all interfaces. GUI only: the headless runner takes QEMU's arguments from its own command line | `src/app.rs`, `App::window_event()`, where the GUI starts QEMU | [10-networking](../10-networking.md), [05-browser-ui](../05-browser-ui.md) |
| `QEMU_NET_DEBUG` | off | Presence | Logs the traffic m-browser handles for the guest to stderr, as `[qemu-net]` lines: ARP, DHCP, pings, each UDP datagram, and TCP connections (connecting, connected, refused, forwarded, closed) | `src/qemu_net.rs`, `Stack::new()`, which passes it on to `TcpNat::new()` in `src/qemu_net/tcp.rs` | [10-networking](../10-networking.md), [14-testing-and-debugging](../14-testing-and-debugging.md) |

## Profiler

The sampling profiler only follows the single-threaded QEMU. With the
multi-threaded one, m-browser prints `QEMU_PROFILE does not follow the
multi-threaded QEMU's vCPU threads` and takes no samples, although
`QEMU_PROFILE` still changes the engine's settings as described below.

| Variable | Default | Values | Effect | Read in | More |
|---|---|---|---|---|---|
| `QEMU_PROFILE` | off | Unset, empty or exactly `0`: off; anything else: on | Samples where QEMU spends its time and prints a `[qemu-profile]` report every `QEMU_PROFILE_EVERY` seconds and when QEMU stops. Turns on wasmtime's epoch checks (a little slower) and turns off source lines in trap reports (function names remain). The first profiled run compiles QEMU's module again, since the cached one was compiled with other settings | `src/qemu_profile.rs`, `settings()`, `configure()` and `start()`, called by `run_wasm_module()` in `src/wasm.rs` | [12-profiling](../12-profiling.md) |
| `QEMU_PROFILE_MS` | 1 | Milliseconds, integer; 0 gives 1 | Time between samples | `src/qemu_profile.rs`, `settings()` | [12-profiling](../12-profiling.md) |
| `QEMU_PROFILE_EVERY` | 30 | Seconds, integer; 0 gives 1 | Length of a report window. Not to be confused with QEMU's own `QEMU_PROF_EVERY` | `src/qemu_profile.rs`, `settings()` | [12-profiling](../12-profiling.md) |
| `QEMU_PROFILE_DIR` | unset | Directory, created if needed | Also writes each window's stacks to `<dir>/window-NNN.folded` (weights in microseconds), the input of flame graph tools such as `inferno-flamegraph` | `src/qemu_profile.rs`, `settings()`, `Profiler::report()` | [12-profiling](../12-profiling.md) |
| `QEMU_PROFILE_TOP` | 25 | Integer | Number of functions in each of the report's two lists (by self time and by total time); 0 lists no functions | `src/qemu_profile.rs`, `settings()`, `Profiler::report()` | [12-profiling](../12-profiling.md) |

## Headless runner

These are for `m-browser --qemu-headless`, which runs QEMU without a window
for tests and scripts; the GUI ignores them.

| Variable | Default | Values | Effect | Read in | More |
|---|---|---|---|---|---|
| `QEMU_SNAPSHOT` | unset | Path of a PNG file | Writes the screen to this file at most once a second, whenever QEMU sends a frame (about once a second even when nothing changes), through a temporary file that is then renamed. The image is the frame as QEMU sends it, magnified by QEMU's `WASI_GFX_SCALE`. Read in the GUI too, but only used where there is no window | `src/qemu_gfx.rs`, `QemuGfxState::new()`; used by `host_gfx_update_framebuffer` in `add_gfx_to_linker()` | [11-headless-and-automation](../11-headless-and-automation.md) |
| `QEMU_TYPE` | unset | `<secs>:<text>` entries separated by `;` | Types each text at its time, about 60 ms per character, both as key presses for the guest and as text for QEMU's text consoles (the monitor, serial consoles); characters with no key on a US keyboard go as text only. Times count from the runner's start, QEMU's start-up included, and may have fractions; entries are sorted by time. In the text, the two-character sequences `\n`, `\t` and `\e` mean Enter, Tab and Escape, so quote the value with single quotes; a text cannot contain `;`. A malformed entry stops the runner with an error | `src/headless.rs`, `run()`, `spawn_typist()` | [11-headless-and-automation](../11-headless-and-automation.md) |
| `QEMU_TYPE_FIFO` | unset | Path of a named pipe (`mkfifo`) | Types each line written to the pipe, followed by Enter. Lines starting with `!` are pointer commands: `!move X Y`, `!click B`, `!down B`, `!up B`, with X and Y in pixels of the frame QEMU sends and B a QEMU button number (0 left, 1 middle, 2 right, 3 and 4 wheel up and down). The pipe is opened again after each writer closes it | `src/headless.rs`, `spawn_fifo_typist()`, `pointer_command()` | [11-headless-and-automation](../11-headless-and-automation.md) |
| `QEMU_QUIT_AFTER` | unset | Seconds, a number that may have fractions | After this long, asks QEMU to quit cleanly (power the guest off, close its disk images), as the GUI does when its window closes; QEMU gets 10 s before it is stopped. Not a number: the runner stops at once with `QEMU_QUIT_AFTER must be a number of seconds`. A negative or infinite value panics the timer thread, and QEMU then keeps running | `src/headless.rs`, `run()` | [11-headless-and-automation](../11-headless-and-automation.md) |

## Standard variables

| Variable | Default | Values | Effect | Read in | More |
|---|---|---|---|---|---|
| `RUST_BACKTRACE` | unset | `1`, `full` | A panic prints the Rust stack. Errors printed in full (`{:?}`), such as `step failed: ...`, also get the Rust stack of where they arose (anyhow; `RUST_LIB_BACKTRACE` controls that part alone). The wasm backtrace of a QEMU trap is printed either way | Rust's standard library, anyhow | [14-testing-and-debugging](../14-testing-and-debugging.md) |
| `RUST_LOG` | - | - | No effect. m-browser depends on env_logger but never starts it (the call in `Wasm::new()`, `src/wasm.rs`, is commented out), so the log messages of its libraries (wgpu, wasmtime, winit) go nowhere | - | - |
| `WAYLAND_DISPLAY`, `DISPLAY` | Set by the desktop session | - | winit, the window library, uses Wayland when `WAYLAND_DISPLAY` (or `WAYLAND_SOCKET`) is set and not empty, else X11 when `DISPLAY` is. Unset `WAYLAND_DISPLAY` to force X11 (XWayland) | winit (Linux) | [05-browser-ui](../05-browser-ui.md) |
| `WASMTIME_BACKTRACE_DETAILS` | unset | `1` | Source lines in the trap reports of wasi-gfx components (the component path, `Wasm::new()`), whose engine follows wasmtime's default. No effect on QEMU, whose engine always has them on (off with `QEMU_PROFILE`) | wasmtime's `Config` | [06-qemu-runner](../06-qemu-runner.md) |

## Variables passed into QEMU

`run_wasm_module()` (`src/wasm.rs`) builds QEMU's WASI environment from
m-browser's: every variable whose name starts with `QEMU_` or `WASI_`
(case-sensitive) is copied, with its value, and no other is, so QEMU sees
no `HOME`, `PATH` or `TMPDIR`. This happens in the GUI and in the headless
runner, and for both QEMU builds: all threads of the multi-threaded one
share one WASI environment. The loop uses `std::env::vars()`, which panics
if any variable in m-browser's environment, not only these, has a name or
value that is not valid UTF-8; QEMU then does not start.

QEMU's own variables, such as `QEMU_JIT_HOT`, `QEMU_TB_SLICE_MS`,
`QEMU_PROF_EVERY`, `WASI_DEBUG` and `WASI_GFX_SCALE`, are documented in
the qemu-wasi repository's docs/reference/environment-variables.md. Set
them on m-browser's command line like the ones above:

```sh
WASI_GFX_SCALE=1 QEMU_JIT_HOT=0 ./target/release/m-browser
```

m-browser's own `QEMU_*` variables are copied too. QEMU does not read
them, so this is harmless. Names that look alike belong to different
programs:

| m-browser | QEMU |
|---|---|
| `QEMU_PROFILE_EVERY`: report window of m-browser's sampling profiler | `QEMU_PROF_EVERY`: QEMU's own coarse time profile (single-threaded build) |
| `QEMU_JIT_DUMP`: every n-th compiled TB module, host path | `QEMU_TB_DUMP`: the module of one TB, chosen by guest address; a path inside QEMU's sandbox |
