# Testing and debugging

**What this covers.** The tests that exist for m-browser's QEMU support,
how to run them and read their results, a manual checklist for the GUI,
and the techniques and switches for finding out why QEMU misbehaves,
ending with a table of known symptoms and their fixes. Read it before
committing a change to the QEMU-facing code, and whenever something
breaks. QEMU's side of testing and debugging is in the qemu-wasi
repository's docs/13-testing.md and docs/14-debugging.md.

## What to run when

| You changed | Run |
|---|---|
| The network (`src/qemu_net.rs`, `src/qemu_net/tcp.rs`) | The unit tests, then the network part of the smoke test |
| The JIT host, the runner, the threads code | The instruction test with both QEMU builds, then the smoke test |
| QEMU's TCG backend (in qemu-wasi) | The instruction test in all three modes, both builds |
| The UI, display or input | The smoke test |
| Anything for speed | The timing scripts in the qemu-wasi repository's tools/perf/ (see its tools/perf/README.md) and [12-profiling.md](12-profiling.md) |

There is no automated test of the GUI. Apart from the timing scripts, only
the instruction test exercises the JIT host and the runner.

## m-browser's unit tests

```sh
# from the m-browser directory
cargo test --release --offline qemu_net        # the 11 tests below
# one test, showing its output:
cargo test --release --offline qemu_net::tests::tcp_relays_both_ways \
    -- --nocapture
```

`qemu_net` selects every test whose path contains it, which is all of
m-browser's tests. They need no network access and no privileges: the TCP
tests run a small guest-side TCP/IP stack (smoltcp) against a real
`VirtualNet` and sockets on 127.0.0.1. `--release` because they move
megabytes; `--offline` needs the crates downloaded already (drop it
otherwise). The first run builds the whole crate in test mode.

| Test | Checks |
|---|---|
| `qemu_net::tests::reassembles_fragments_in_any_order` | IP fragments from the guest reassemble whatever their order |
| `qemu_net::tests::fragments_to_the_guest_reassemble` | A 4,000-byte packet for the guest becomes fragments of at most 1,500 bytes, with valid checksums, that reassemble |
| `qemu_net::tests::checksum_matches_rfc1071_example` | The Internet checksum matches RFC 1071's example, also split at an odd offset |
| `qemu_net::tests::dhcp_options_stop_at_end_or_truncation` | The DHCP option parser stops at the end option and at truncated data |
| `qemu_net::tests::tcp_relays_both_ways` | 1 MB to a host echo server and back, then a close the host sees |
| `qemu_net::tests::tcp_refused_connection_is_reset` | A connection to a closed host port is reset, never established |
| `qemu_net::tests::tcp_half_close_from_the_host` | The host sends 300 KB and closes first; the guest, reading slowly, gets all of it and the close, then answers |
| `qemu_net::tests::tcp_parallel_connections` | 20 simultaneous connections each get a host connection of their own |
| `qemu_net::tests::tcp_port_forward_into_the_guest` | A `hostfwd` rule: a host connection reaches a guest listener, and its answer comes back |
| `qemu_net::tests::tcp_host_reset_while_guest_sends` | A host reset during a transfer resets the guest's connection, and new connections still work |
| `qemu_net::tcp::tests::hostfwd_rules` | Valid and invalid `hostfwd` rule strings are parsed correctly |

## The instruction test (in qemu-wasi)

A differential test: the same x86 program runs on a native QEMU and on the
wasm QEMU, and their results must be identical. It is the main safety net
for the TCG backend and for m-browser's JIT host.

Needs gcc for x86-64, a native QEMU built from the same patched tree
(`make native`, which needs QEMU's usual build dependencies such as
glib's development package), and m-browser built with
`cargo build --release` in `../m-browser` (or set `M_BROWSER`).

```sh
# from the qemu-wasi directory
make native                         # once
make test                           # single-threaded build, qemu-build/
make THREADS=1 test                 # multi-threaded, qemu-build-threads/
QEMU_JIT_HOT=0 make test            # JIT only: each TB compiled at once
QEMU_JIT_HOT=4000000000 make test   # interpreter only: no TB gets hot
```

What `tests/x86-insn-diff/run.sh` does:

1. Builds `insn-test.c` into a tiny bare-metal kernel with QEMU's test
   library: it runs several hundred instruction forms (351 when last
   counted) on many inputs and prints a hash of the results per form to
   the debug port (0xe9).
2. Runs it on the native QEMU (at most 120 s) and on the wasm QEMU through
   `m-browser --qemu-headless` (at most `WASM_TIMEOUT`, default 900 s),
   both with `-M pc -cpu max -m 64 -smp 1`.
3. Compares the two outputs.

It tests the wasm build in `qemu-build*/` directly, not the copy installed
in m-browser, but with m-browser's current runner, so both repositories'
code is under test. The `QEMU_JIT_HOT` setting reaches QEMU because
m-browser forwards `QEMU_*` variables.

| Output | Meaning |
|---|---|
| `native: N lines, wasm: N lines` then `PASS: all instruction hashes match` | Every instruction form gives the same results |
| `FAIL: mismatching instruction forms:` and a list (exit status 1) | Those forms compute something differently under wasm: a bug in code generation, in the interpreter, or in how TBs are compiled and run |
| Fewer wasm lines than native ones | The wasm run crashed or timed out: read `wasm-run.log` |

The interpreter runs the very wasm that the JIT compiles, so the failing
mode tells you where to look. Only JIT-only fails: compiling or running
the modules (m-browser's JIT host, wasmtime). Only interpreter-only fails:
the interpreter, `tcg/wasm32/wasi-interp.c.inc`. Both fail: the generated
wasm itself (the backend's code generation) or code both tiers share, such
as helpers. Add `THREADS=1` to any of them; the test then runs one vCPU,
but through `wasi.thread-spawn`, `host_vcpu_run` and a vCPU Store.

Files, in `OUT` (default `${TMPDIR:-/tmp}/qemu-wasi-insn`):
`insn-test.elf`, `native.txt`, `wasm.txt`, `diff.txt`, `wasm-run.log`
(m-browser's output for the wasm run).

## Smoke test of the GUI

Run it after any change that reaches QEMU. Times are from the development
machine, with the multi-threaded build and 4 vCPUs.

1. Build both: `make THREADS=1 install` in qemu-wasi, then
   `cargo build --release --offline` in m-browser.
2. From the m-browser directory, keep a log:
   `./target/release/m-browser 2>&1 | tee /tmp/m-browser.log`.
3. Open `file:///<path to m-browser>/qemu-system-x86_64.wasm`. The log
   shows `Multi-threaded QEMU: one thread per vCPU`, `4 vCPUs (QEMU_SMP)`
   (2 on small hosts), `Running` and `[qemu-gfx] host_gfx_init(...)`. The
   first start after a rebuild also compiles QEMU once.
4. Boot: the Alpine desktop appears after about 2.5 minutes.
5. Keyboard: click into QEMU's window (not the address bar), open a
   terminal, type, try Shift and Ctrl+C.
6. Mouse: the guest pointer follows the host's exactly; left and right
   clicks work; the wheel scrolls.
7. Monitor: Ctrl+Alt+2 shows `(qemu)`; `info status` answers
   `VM status: running`; Ctrl+Alt+1 returns to the guest.
8. Serial console: Ctrl+Alt+3 shows the guest's first serial port;
   Ctrl+Alt+1 returns.
9. Network, in a guest terminal: `ip addr` shows 10.0.2.15;
   `ping -c 3 10.0.2.2` answers; `wget -O /dev/null http://example.com`
   works (DNS and TCP); `apk update` (as root) fetches the package index.
10. Chromium: start it from the desktop. In the reference timings its
    window appeared about 3:55 after QEMU started, and a click made while
    it starts is handled within about 10 s (6 to 9 s measured).
11. Clean shutdown: `poweroff` in the guest (QEMU then exits with status
    0), or navigate away or close the browser (`asking QEMU to quit`, then
    `exited with status 0`). Start again: no `in use by another QEMU`.

## Debugging techniques

### Where the output goes

QEMU's stdout and stderr are m-browser's (`inherit_stdio()`), so start the
GUI from a terminal and capture both streams, as above.

| Prefix | Source |
|---|---|
| `[qemu] [  t s]` | The runner's progress, with seconds since start (`src/wasm.rs`) |
| `[qemu]` | Runner and threads (`src/wasm.rs`, `src/qemu_threads.rs`) |
| `[qemu-jit]`, `[qemu-gfx]`, `[qemu-net]`, `[qemu-profile]` | JIT host, display, network, profiler |
| `[headless]`, `[gfx]`, `[detect]` | Headless runner; browser window and wasm detection (`src/app.rs`) |
| `qemu-system-x86_64: ...` | QEMU's own error messages |
| `[qemu-wasi]`, `qemu-wasi:`, `[wasi-interp]`, `[wasi-prof]`, `wasi-gfx:` | QEMU's WASI port: TB buffer flushes, JIT failures, interpreter and profile reports, display |
| `WASI_DBG:`, `QEMU_STATUS:` | `WASI_DEBUG` and `QEMU_STATUS_EVERY` output |

### Traps and backtraces

When QEMU traps (a failed assertion, `abort()`, an out-of-bounds access),
the error printed after `step failed:`, `thread N failed:` or `vCPU N
failed:` contains a wasm backtrace: one frame per line, innermost first.
Function names come from the module's name section, which the builds
keep; source files and lines come from DWARF debug info, which the engine
uses (`WasmBacktraceDetails::Enable`) and QEMU's build includes (`-g`). A
build without debug info still shows names. Frames inside TB modules have
no useful names. QEMU's own message, if any, comes just before.
`RUST_BACKTRACE=1` adds native backtraces to Rust panics.

### Switches

| Variable | Read by | Effect |
|---|---|---|
| `QEMU_JIT_STATS=1` | m-browser | JIT statistics every minute ([06-qemu-runner.md](06-qemu-runner.md)) |
| `QEMU_NET_DEBUG=1` | m-browser | Logs every packet the network handles |
| `QEMU_JIT_DUMP=<dir>[,<n>]` | m-browser | Saves every n-th compiled TB module ([07-jit-host.md](07-jit-host.md)) |
| `QEMU_PROFILE=1` | m-browser | Sampling profile, single-threaded build ([12-profiling.md](12-profiling.md)) |
| `WASI_DEBUG=1`, `2` | QEMU | Start-up and lifecycle tracing; 2 adds every TB (very slow) |
| `QEMU_STATUS_EVERY=<s>` | QEMU | vCPU state and recent TBs every s seconds |
| `QEMU_PROF_EVERY=<s>` | QEMU | Coarse time profile, single-threaded build |
| `QEMU_TB_DUMP=<pc>,<dir>` | QEMU | The module of the TB at guest address pc |
| `QEMU_JIT_HOT=0`, `4000000000` | QEMU | JIT only, interpreter only: tells the two tiers apart |
| `QEMU_SMP=1` | m-browser | One vCPU, to rule out threading bugs |

The complete list is in
[reference/environment-variables.md](reference/environment-variables.md).

### QEMU's monitor and the serial console

The GUI starts QEMU with `-monitor vc -serial vc`. Ctrl+Alt+2 shows the
monitor, Ctrl+Alt+3 the guest's serial port, Ctrl+Alt+1 the guest's
screen again.

| Monitor command | Shows or does |
|---|---|
| `info status` | Whether the guest runs (`VM status: running`) |
| `info registers`, `info registers -a` | The registers of the current vCPU, of all vCPUs |
| `info cpus` | The vCPUs and where they are |
| `info network` | The network card and the user network ("provided by the WASI host") |
| `info block` | Disks and their image files |
| `system_powerdown` | Presses the guest's power button |
| `quit` | Ends QEMU at once, closing its disk images |

What the serial console shows depends on the guest: kernel messages if its
kernel logs to ttyS0, a login prompt if a getty runs there. To reproduce a
problem without the GUI, or to capture the screen and the serial port to
files, use the headless runner
([11-headless-and-automation.md](11-headless-and-automation.md)).

## Symptoms, causes and fixes

| Symptom | Cause | Fix |
|---|---|---|
| The browser panics with `Failed to acquire next swap chain texture`, killing the VM | Builds before commit 3c091ad: the host's screen blanked and wgpu timed out | Fixed: such frames are skipped (`[gfx] no swap chain texture (...); skipping frame`). Update m-browser ([05-browser-ui.md](05-browser-ui.md)) |
| `m.qcow2 is in use by another QEMU; not starting a second one on the same disk image` | Another m-browser process runs a QEMU on this image and holds its lock | Close it; `fuser -v m.qcow2` finds it. The headless runner takes no lock: never point it at an image in use |
| QEMU's window stays black | The guest's X screen saver blanked the screen after 10 minutes without input | Move the mouse over the window or press a key; `xset s off -dpms` in the guest turns blanking off. Early in boot, or on a text console (Ctrl+Alt+1), black is normal too |
| Keys or clicks lost while the guest is busy | The single-threaded build (its one vCPU is busy), or too little guest RAM (X stalls while swapping and the guest kernel drops keys) | Use the multi-threaded build (`make THREADS=1 install`); keep `-m 1024`; type again once the guest is idle |
| The process aborts with ENOMEM though RAM is free | More memory mappings than `vm.max_map_count` | See [06-qemu-runner.md](06-qemu-runner.md): watch `QEMU_JIT_STATS`, lower `QEMU_MIGRATE_AFTER`, or `sudo sysctl -w vm.max_map_count=262144` |
| `QEMU exited during startup with status N (its error message is above)` | QEMU rejected an option or could not open a file | Read QEMU's message just above. Usual causes: not started from the m-browser directory (no `m.qcow2`, no `pc-bios/`), a damaged image, a bad `QEMU_NIC` |
| Pings to the internet fail; `[qemu-net] cannot ping from the host: ...` | Linux allows unprivileged ping sockets only to groups in `net.ipv4.ping_group_range` | `sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"` (until reboot). Pings to 10.0.2.2 and 10.0.2.3 work regardless |
| `[qemu] thread N failed`, `[qemu] vCPU N failed` or `QEMU exited with status N (vCPU N)`, then the whole process exits | A QEMU thread trapped or called `exit()`; wasi-threads ends the program ([08-threads.md](08-threads.md)) | Read the error and backtrace; retry with `QEMU_SMP=1` and with the single-threaded build |
| `QEMU_PROFILE` prints a notice and no profile | The multi-threaded build | Profile the single-threaded build ([12-profiling.md](12-profiling.md)) |
| Slow boot | First start after a new QEMU build or a wasmtime update (`compiling QEMU's module...`); m-browser built without `--release`; Xorg's glamor acceleration in the guest (about 30 minutes, see the qemu-wasi README); frequent `[qemu-wasi] TB buffer flush`; `WASI_DEBUG=2` or `QEMU_PROFILE` set; a small host (fewer vCPUs and compiler threads) | Fix the cause; with the multi-threaded build, expect the desktop after about 2.5 minutes on a fast host |
| The instruction test fails | A backend, interpreter or JIT host bug | Narrow it down with the JIT-only and interpreter-only modes (above) |
| Keys typed go to the browser, not the guest | A browser text field (the address bar) has the keyboard focus | Click elsewhere so that it loses focus ([09-display-and-input.md](09-display-and-input.md)) |
