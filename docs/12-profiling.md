# Profiling QEMU: where does the time go?

**What this covers.** The sampling profiler built into m-browser
(`src/qemu_profile.rs`): how it works, how to switch it on, how to read its
report and turn its output into flame graphs, what it cannot see (in
particular the multi-threaded QEMU), and the project's other timing tools.
Read it before optimising QEMU or m-browser's host code, so that you
measure first.

## Summary

```text
ticker thread "qemu-profile"          QEMU's thread (single-threaded build)
every QEMU_PROFILE_MS (1 ms):         compiled wasm checks the epoch at every
  TICK += 1                             function entry and loop header
  engine.increment_epoch()  -------->   deadline passed -> callback:
                                          capture the wasm call stack and
                                          charge it the time since the
                                          previous sample
                                      call hook: times every host call
every QEMU_PROFILE_EVERY (30 s): report on stderr (+ window-NNN.folded)
```

A *sampling profiler* looks many times a second at what the program is
doing and adds up where it finds it. The *call stack* (backtrace) is the
list of functions currently running, each called by the one before.
*Self time* is time during which a function was the innermost one; *total
time* is time during which it was anywhere on the stack.

## Quick start

1. Use the single-threaded QEMU ([why](#limitation-single-threaded-build-only)).
   `make` in the qemu-wasi directory builds it as
   `qemu-build/qemu-system-x86_64`.
2. Run it headless with the profiler on. The runner takes any module
   path, so nothing needs installing (this assumes the two repositories
   side by side, and a disk image copy as in
   [11-headless-and-automation.md](11-headless-and-automation.md)):

   ```sh
   # from the m-browser directory
   mkdir -p /tmp/prof
   QEMU_PROFILE=1 QEMU_PROFILE_DIR=/tmp/prof QEMU_QUIT_AFTER=600 \
   ./target/release/m-browser --qemu-headless --preopen "$HOME/vm-test=/out" \
       ../qemu-wasi/qemu-build/qemu-system-x86_64 -L /bios \
       -drive file=/out/disk.qcow2,format=qcow2 -m 1024 \
       -accel tcg,tb-size=256 -device qemu-xhci -device usb-tablet \
       -monitor vc -serial vc -nic user 2> /tmp/prof/stderr.log
   ```

3. Read the reports with `grep qemu-profile /tmp/prof/stderr.log`, and
   make flame graphs from `/tmp/prof/window-*.folded` ([below](#flame-graphs)).

**Note:** the first profiled run compiles QEMU's module again, which takes
a while: the cached compiled module depends on the engine settings, and
profiling changes them.

## How it works

1. `configure()`, called by `run_wasm_module()`, turns on
   `Config::epoch_interruption()` and turns off `wasm_backtrace_details`
   (looking every frame up in QEMU's DWARF debug information a thousand
   times a second would be far too slow; names come from the module's
   name section instead).
2. `start()` runs on QEMU's thread. It starts the ticker thread
   `qemu-profile`, which every `QEMU_PROFILE_MS` increments `TICK` and
   calls `Engine::increment_epoch()`, and creates the thread-local
   `Profiler`.
3. `attach()`, called for every Store QEMU runs in (the first one and
   each one made by `migrate_store()`), sets an epoch deadline of one
   tick and installs an epoch-deadline callback (`sample()`, then
   `UpdateDeadline::Continue(1)`) and a call hook (`on_transition()`).
4. All code compiled with these settings checks the epoch at each
   function entry and loop header: QEMU's own module and every translation
   block (TB) module (QEMU turns each piece of guest code it translates
   into a small wasm module). Once the deadline has passed, wasmtime calls
   the callback on QEMU's thread, with QEMU's frames still on the stack.
5. `Profiler::sample()` captures the stack (`WasmBacktrace::force_capture`)
   and names its frames: QEMU's functions by name (`func[N]` if
   unnamed), frames of any other module, that is TB modules compiled from
   guest code, as `[guest code]`. It splits the time since the previous
   sample: host-call time goes to `[host: ...]` frames pushed on top of
   the stack, time outside wasm to a one-frame stack `[outside QEMU]`,
   and the rest to the stack itself.
6. The call hook adds up time spent in host calls, counts them, and
   measures time outside wasm between QEMU's main-loop steps. When a host
   call returns and a tick has passed since the last sample, it samples
   right there, while the caller is on the stack: a long host call always
   spans a tick, and is charged to the code that made it.
7. Every `QEMU_PROFILE_EVERY` seconds `report()` prints the window and
   starts the next. When QEMU stops, `stop()` (from a drop guard in
   `run_wasm_module()`) prints the last, partial window and ends the
   ticker.

Host time is split into categories. Host functions mark their work with
`time_host(HostWork::...)`, a guard that times until dropped, or
`add_host_work()`:

| Frame | What | Measured in |
|---|---|---|
| `[host: idle wait]` | QEMU sleeping because the guest is idle | `host_net_wait` (`HostWork::IdleWait`) |
| `[host: JIT compile]` | Cranelift compiling a TB module on QEMU's thread | `JitCache::get_or_compile()` (`HostWork::JitCompile`) |
| `[host: JIT instantiate]` | The rest of a JIT request: cache lookups, instantiation, table update | `jit_load()` (`HostWork::JitTotal` minus compile time) |
| `[host: display]` | Converting and showing a frame | `host_gfx_update_framebuffer` (`HostWork::Display`) |
| `[host: other (WASI files, clocks...)]` | All other host time: disk reads and writes, clocks, network, input polling | Host time minus the above |
| `[outside QEMU]` | m-browser's own work between main-loop steps, such as moving QEMU to a fresh store | The call hook |

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `QEMU_PROFILE` | unset | Any value except empty or `0` turns the profiler on |
| `QEMU_PROFILE_MS` | 1 | Sampling interval in milliseconds (at least 1) |
| `QEMU_PROFILE_EVERY` | 30 | Length of a report window in seconds (at least 1) |
| `QEMU_PROFILE_DIR` | unset | Write each window's stacks to this directory (created if needed) |
| `QEMU_PROFILE_TOP` | 25 | Rows in each table of the report |

## Reading the report

An example, with made-up numbers:

```text
[qemu-profile] sampling every 1ms, reporting every 30 s, stacks in /tmp/prof
[qemu-profile] window 3 (60-90 s): 29102 samples, 48211 host calls/s
[qemu-profile]   guest code 18.2%, QEMU's own code 47.5%, host calls 33.6% (idle 2.1, JIT compile 21.4, JIT instantiate 1.9, display 3.0, other 5.2), outside QEMU 0.7%
[qemu-profile]    self%  total%  function (by self time)
[qemu-profile]    21.40   21.40  [host: JIT compile]
[qemu-profile]    18.20   18.20  [guest code]
[qemu-profile]     9.75   30.90  tcg_qemu_tb_exec
...
[qemu-profile]    self%  total%  function (by total time)
[qemu-profile]     0.00   99.30  wasi_qemu_step
...
```

| Line | Meaning |
|---|---|
| `sampling every ...` | Printed once at the start |
| `window N (FROM-TO s): S samples, H host calls/s` | Window number, seconds since profiling started, number of samples, host calls per second |
| `guest code ...` | How the window's time splits: *guest code* is compiled TB code; *QEMU's own code* is all other wasm (main loop, device emulation, the interpreter, helpers that TB code calls); *host calls* is all `[host: ...]` time, broken down in parentheses (each figure a percentage of the whole window, adding up to the host total); *outside QEMU* is m-browser's work between steps |
| first table | The `QEMU_PROFILE_TOP` functions with the most self time |
| second table | The same, ordered by total time |

All percentages are of the time measured in the window. The outermost
functions, such as `wasi_qemu_step`, are on nearly every stack, so their
total is close to 100%: read the second table downwards to see how the
time divides. A `[host: ...]` frame sits on top of the QEMU function that
made the call, so that function's total includes the host time and its
self time does not.

## Flame graphs

With `QEMU_PROFILE_DIR` set, each window's stacks also go to
`<dir>/window-NNN.folded` (`NNN` is the window number). Each line is one
distinct stack, outermost frame first, frames separated by `;`, then a
space and the time in microseconds (integers, as flame graph tools want):

```text
wasi_qemu_step;...;tcg_qemu_tb_exec;[guest code] 5462110
```

A flame graph draws these stacks as nested bars whose width is time. The
tools are not bundled; two common ones:

```sh
cargo install inferno           # once, needs network access
inferno-flamegraph /tmp/prof/window-003.folded > window-003.svg

git clone https://github.com/brendangregg/FlameGraph   # once
FlameGraph/flamegraph.pl /tmp/prof/window-003.folded > window-003.svg
```

Open the SVG in a web browser: click to zoom, search with Ctrl+F. Both
tools label the widths "samples"; here they are microseconds. To cover
several windows, concatenate the files:
`cat /tmp/prof/window-*.folded | inferno-flamegraph > all.svg`.

## Interpreting results

**Pitfall:** samples land only at function entries and loop headers. Time
in straight-line code is charged at the next such point, possibly in
another function. A compiled TB is mostly straight-line code, so much of
its time lands on the loop that runs one TB after another and that each TB
returns to, `tcg_qemu_tb_exec`: read `[guest code]` and
`tcg_qemu_tb_exec`'s self time together as "running guest code".

**Pitfall:** the profiler slows QEMU down by roughly 25-40% (the epoch
checks in every function and loop, a stack capture per sample, the call
hook on every host call). Timing-dependent behaviour shifts with it:
compare profiles with profiles, not with unprofiled timings.

**Note:** TBs that QEMU still interprets (it runs a TB in its interpreter
until it has run often enough to be compiled) count as QEMU's own code,
not `[guest code]`. Inlined functions do not appear at all: the compiler
merged them into their callers. Only QEMU's thread is sampled, so
background compiles (`wasi_jit_load_async`), the network thread and the
UI do not appear.

## Limitation: single-threaded build only

The profiler follows one thread. With the multi-threaded QEMU, which is
the build installed by default, `run_wasm_module()` prints

```text
[qemu] QEMU_PROFILE does not follow the multi-threaded QEMU's vCPU threads
```

and gives no report: the vCPUs run on threads with Stores of their own,
which `start()` and `attach()` never see, and `QemuThreads::new_store()`
sets their epoch deadline out of reach. The epoch checks are still
compiled in, so unset `QEMU_PROFILE` there.

To profile, use the single-threaded build: headless, run qemu-wasi's
`qemu-build/qemu-system-x86_64` directly (Quick start); for the GUI,
install it temporarily:

```sh
# in the qemu-wasi directory
make install             # single-threaded build into ../m-browser
# ... profile with QEMU_PROFILE=1 ./target/release/m-browser ...
make THREADS=1 install   # back to the multi-threaded build
```

Keep in mind that the two builds schedule work differently (the
single-threaded one runs the vCPU in time slices inside the main loop),
so a profile of one does not describe the other in every detail.

**Pitfall:** extending the profiler to threads takes more than calling
`attach()` on every Store. Its state is thread-local, and a Store has a
single call hook, which the threaded build already uses to release the
shared WASI lock (`install_store_hooks()` in `src/qemu_threads.rs`);
`attach()` would replace it.

## Adding a host-work category

1. Add a variant to `HostWork` and raise `HOST_WORK_KINDS`.
2. Extend the `WORK_NS` initialiser and, in `Profiler::new()`,
   `host_frames` with the new frame's name.
3. In `Profiler::sample()`, read the new counter, subtract it from
   `other`, and add it to the list of host frames charged.
4. Add it to the breakdown printed by `report()`.
5. In the host function, start with
   `let _timer = crate::qemu_profile::time_host(HostWork::NewKind);`.
   With the profiler off this costs next to nothing.

## Other timing tools

| Tool | Where | What it shows |
|---|---|---|
| `QEMU_PROF_EVERY=<seconds>` | QEMU itself, qemu-wasi's `tcg/wasm32.c` | A coarse profile (`[wasi-prof]` lines on stderr): time spent translating, interpreting, compiling and running TBs, in the main loop and idle. Single-threaded build only; see the qemu-wasi repository's docs/15-performance.md |
| `QEMU_JIT_STATS=1` | `src/wasm.rs` | JIT statistics every minute: compiles, cache hits, memory mappings |
| `QEMU_DISPLAY_STATS=1` | QEMU's display backend | Cost of display refreshes, every ~20 s |
| Guest-side logging | qemu-wasi's `tools/perf/` | A logger inside the guest whose output is recorded from the serial port, and scripts that use the headless runner to measure boot time, input latency and guest speed from outside; see tools/perf/README.md there |
