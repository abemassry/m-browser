# Known issues and limitations (m-browser)

**What this covers.** Problems, limitations and rough edges in m-browser
that are known but not fixed, collected while writing this documentation
and during the QEMU work.  Each entry says how it shows and why.  Check
here before debugging something strange; add to it when you find
something you cannot fix right away.  Items marked "(from reading the
code)" were found by reading, not by running.  QEMU-side issues are in the
qemu-wasi repository's docs/known-issues.md.

## Affects users

- **A failing QEMU thread ends the whole browser** (multi-threaded QEMU).
  If any QEMU thread other than the main one traps or calls `exit()`,
  `thread_failed()` in `src/qemu_threads.rs` exits the process: that is
  the wasi-threads rule (a thread's failure ends the program), and there
  is no safe way to continue.  See [08-threads.md](08-threads.md).
- **Stopping QEMU leaks memory** (multi-threaded QEMU, from reading the
  code).  QEMU's vCPU threads stay blocked after QEMU exits (see the
  qemu-wasi known issues), and `QemuThreads` keeps its `InstancePre`, whose
  host functions hold `QemuThreads` again (a reference cycle): QEMU's
  shared memory, the JIT cache, the compiler threads and the network
  (with its port forwards) are freed only when the browser exits.
  Starting QEMU again in the same session costs the memory again, and
  fails if it forwards a host port the old network still holds (see
  [10-networking.md](10-networking.md)).
- **Closing the browser waits only 5 s for QEMU**
  (`App::shutdown_qemu()` in `src/app.rs`), while QEMU may take up to 10 s
  (`QUIT_TIMEOUT` in `src/wasm.rs`) to power off and close the disk image.
  Normally it takes far less; the image's journal covers the rare case.
- **A click in the QEMU window may also reach the browser's UI** (from
  reading the code): egui receives pointer events from the child window as
  if they happened in the main window, so a click could focus the address
  bar or press a tab button (which navigates away and stops QEMU).
  Untested.
- **The QEMU window is never resized.**  It is 1080x720 logical pixels; on
  a display with scale factor 1, the guest's 1280x800 screen is cut off.
  On scale factor 2 it fits.
- **A missing local file or a failed `.wasm` download panics** the browser
  (`navigate()` and `download_wasm()` in `src/app.rs` use `unwrap`).
- **Relative links in Markdown pages are not resolved**, so they fail to
  load.
- **Non-UTF-8 environment variables make QEMU fail to start**:
  `std::env::vars()` in `run_wasm_module()` panics on them.
- **On Wayland the QEMU window may not be embedded** in the browser
  window (winit's parent-window support; untested).

## Headless runner

- **No disk-image lock**: never run it on `m.qcow2` while the GUI uses it
  (two QEMUs corrupt a qcow2 image).  qemu-wasi's tools/perf scripts use
  copies.
- **Exit status is 0** whatever QEMU's own exit status was.
- **Tab and Backspace are typed twice on QEMU's text consoles** (sent both
  as key presses and as text).
- **`QEMU_TYPE_FIFO` pointing at a regular file** retypes it forever (it
  is reopened after each end of file); use a named pipe (`mkfifo`).
- **A negative `QEMU_QUIT_AFTER`** panics its timer thread, so QEMU is
  never asked to quit.
- **The typist sends about 16 characters a second, open loop**: keys
  typed while the guest is very busy can be lost (the guest's keyboard
  buffers are small).

## JIT, runner and profiler

- **`QEMU_PROFILE` does nothing useful with the multi-threaded QEMU** (the
  one m-browser ships): it does not follow vCPU threads, though it still
  turns on epoch checks (and so its own module cache entry).  Profile with
  the single-threaded QEMU ([12-profiling.md](12-profiling.md)).
- **`QEMU_JIT_DUMP` misses modules with background compiling**: modules
  collected in the same batch get the same number and overwrite each
  other.
- **`QEMU_JIT_STATS` is incomplete**: the per-size line only counts
  synchronous compiles (empty by default), and with the multi-threaded
  QEMU "loads shared an instance" and "instances in the store" read 0
  (they come from the main thread's Store, where no translated code runs).
- **A module that fails to instantiate in background mode** returns 0
  (not "failed") to QEMU, which retries every 16 runs and logs each time.
- **The `-smp` check** in `run_wasm_module()` only recognizes the exact
  argument `-smp`; and `-smp 9` or more makes QEMU abort (it has 8
  per-vCPU slots).
- **`RUST_LOG` has no effect**: env_logger is never initialized.

## Networking

- **`QEMU_NET_DEBUG` is enabled by its mere presence**, even `=0`.
- **`host_net_recv` removes a frame before checking it fits** QEMU's
  buffer, so an oversized frame would be lost silently.  Cannot happen
  today (frames are at most 1514 bytes, the buffer 69632).
- **TCP to internet addresses** relies on smoltcp answering for any
  address without routes; it works (`wget`, `apk`), but no automated test
  covers it (the tests use 10.0.2.2).
- Unsupported: IPv6, UDP port forwarding (designed in
  [13-adding-features.md](13-adding-features.md)), forwarding rules added
  at run time.  See [10-networking.md](10-networking.md).

## Code quality notes

- `src/fill.rs` is a no-op (its real code needs the unused `rwh_05`
  feature); `WasiWinitEventLoop` in `src/winit_wasi.rs` and the `back()` /
  `forward()` functions in `src/app.rs` are unused or stubs
  ([05-browser-ui.md](05-browser-ui.md)).
- `App::set_window()` passes the width for both width and height;
  redraws are not filtered by window; "Window is minimized" is printed on
  every frame while minimized.
- The swap-chain "skipping frame" message is printed on the 1st, 2nd,
  3rd, 5th, 9th, 17th... occurrence (the count is tested before it is
  incremented).
- Stale comments: the module comment at the top of `src/qemu_jit.rs`
  lists `wasi_jit_call` and `wasi_jit_drop` as the interface; the comment
  at the `add_net_to_linker` call in `src/wasm.rs` mentions only
  `host_net_send` and `host_net_recv`; `InputEvent`'s comment in
  `src/qemu_gfx.rs` omits event type 5 (text); the `Stack.debug` comment
  in `src/qemu_net.rs` says `QEMU_NET_DEBUG=1`.
- Unused imports and fields produce compiler warnings (`cargo build`
  lists them).
