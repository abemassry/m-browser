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
- **Closing the browser waits only 5 s for QEMU** (`App::shutdown()` in
  `src/app.rs`, 5 s in all), while QEMU may take up to 10 s
  (`QUIT_TIMEOUT` in `src/wasm.rs`) to power off and close the disk image.
  Normally it takes far less; the image's journal covers the rare case.
- **Only one tab at a time can boot `m.qcow2`.**  A
  `qemu-system-x86_64.wasm` page boots `m.qcow2`, which only one QEMU may
  write; a second one waits 12 s for the image, then gives up with
  "m.qcow2 is in use by another QEMU" in the status bar.  Web tabs are
  not limited: each runs from the web checkpoint on an overlay of its own
  (only memory limits them, about 1.5 to 2 GB each).
- **A web tab's first page takes up to a minute** to appear: a QEMU
  resumed from a checkpoint has no translated code yet and translates
  Chromium's again.  Later pages are faster.
- **Web tabs lose their page state over a restart**: they come back by
  address and reload (a checkpoint per tab would take about 0.9 GB).
  Logins kept only in Chromium's memory are gone; the guest's disk
  changes of a web tab are dropped with its overlay when the tab's QEMU
  stops.
- **The web checkpoint takes space**: its copy of `m.qcow2` (about 2 GB,
  partly sparse) and its state (about 0.9 GB).  Making a new one deletes
  the old pair.  The web setup needs `m.qcow2` not in use while it copies
  it.
- **The setup tab is not kept over a restart**: quitting during the
  setup, before "Save web checkpoint", means setting up again (its copy
  is deleted).
- **The address typed into a web tab's Chromium uses a US keyboard
  layout**; characters with no key on it (accented letters, for one) are
  left out.
- **Waiting for a web tab's Chromium is blind**: the address is typed as
  soon as the checkpoint is loaded, which works for a checkpoint saved
  with Chromium idle and its window focused, as the setup asks.  A
  checkpoint saved in another state gets the keys wherever its focus is.
- **A paused guest's clock falls behind** by the time its tab was in the
  background, until the guest corrects it (NTP); its network connections
  may have timed out meanwhile.
- **A core module that only computes is not paused** in the background:
  it pauses at its next call to a WASI function m-browser wraps, and one
  that makes none runs on.  Reactors (no `_start`) do not run in a tab.
- **A component could die after its tab comes back** (from reading the
  code, not seen): wasi-webgpu-runtime unwraps the swap chain's texture,
  so a swap chain that reports itself outdated after the window was
  hidden would panic its host call.  The status bar then says that the
  host code panicked, and the tab needs Reload.
- **Programs that never return to wasm cannot be stopped.**  A component
  waiting for input in a host call when its tab is closed keeps its
  thread, and its hidden window, until the browser exits.
- **A component cannot follow the window's size.**  wasi-webgpu-runtime
  configures a component's swap chain once, at the size of its window
  when the component starts, and has no resize path; so a component's
  window keeps that size (1080x720 logical pixels, or the page area if
  smaller) and does not grow with the browser window.
- **The guest's screen is scaled with nearest neighbour** to fit the page
  area: by a factor that is not a whole number, text looks uneven.  The
  guest's resolution does not follow the window (QEMU ignores resize
  requests).
- **A QEMU window may show stale pixels for up to a second** when it first
  appears (it shows once QEMU runs, and QEMU paints it at display
  start-up), longer while a new QEMU build is compiled.
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

## Saved states and the generic runner

See [15-saved-states.md](15-saved-states.md).

- **Only core modules run by `--wasm-headless`, and QEMU in
  `--qemu-headless`, can be saved so far**: the GUI and wasi-gfx
  components cannot (pausing in the GUI, and QEMU's checkpoints there,
  are planned).
- **Only the multi-threaded QEMU can save**: QEMU writes its state from a
  migration thread, and the single-threaded build's threads never run.
  It refuses `!save` with a message; it can pause.
- **After a QEMU save, the disk named in `-drive` is frozen**: QEMU
  writes to new overlays in `QEMU_SAVE_DIR` from then on. Booting that
  disk afresh writes it, and the checkpoints that need it then refuse to
  resume. Overlays pile up (one per disk, per save and per resume) and
  are never removed.
- **A resumed QEMU's host-side state starts afresh**: the guest's network
  connections are gone, and files in its arguments other than disks (a
  `-serial file:` log) are opened anew by every copy, at the same path.
- **QEMU checkpoints check only writable disks**: read-only disks and
  CD-ROMs are neither frozen nor checked on resume.
- **Components can never be saved generically with wasmtime 41**: a
  component's resource handles (GPU device, textures, subscriptions) are
  numbers whose table lives inside wasmtime with no API, and its core
  instances are not reachable either.
- **Threads cannot be saved**: modules with a shared memory are refused
  (their threads' stacks are always live).
- **A command needs Asyncify to be saved** (`wasm-opt --asyncify`), which
  makes it larger (61% and 75% measured on the wasi-gfx samples' main
  modules) and somewhat slower (not measured), and does not work on
  modules using wasm exceptions.
- **Save points of commands are the wrapped WASI calls only**: a program
  computing in a loop without calling one of them can be neither paused
  nor saved until it does.
- **Modules that change their tables** (`table.set`, `table.grow`, ...)
  run but cannot be saved.
- On resume, a module's start function (if it has a start section) runs
  again, and data and element segments it had dropped are back.
- **Programs that close a preopened directory or renumber standard I/O**
  cannot be saved (`cannot hibernate: its open files cannot be saved`).
- **A hibernated state can be resumed more than once** while the
  program's files are unchanged; nothing marks it as used.
- **The WASI wrappers use code wasmtime-wasi calls internal**
  (`wasmtime_wasi::p1::wasi_snapshot_preview1`); a wasmtime upgrade may
  need them changed.
- **`WASM_CONTROL_FIFO` pointing at a regular file** reads it again and
  again; use a named pipe.

## Code quality notes

- `WasiWinitEventLoop` in `src/winit_wasi.rs` is unused
  ([05-browser-ui.md](05-browser-ui.md)).
- `App::set_window()` passes the width for both width and height;
  "Window is minimized" is printed on every frame while minimized.
- The swap-chain "skipping frame" message is printed on the 1st, 2nd,
  3rd, 5th, 9th, 17th... occurrence (the count is tested before it is
  incremented).
- Stale comments: the module comment at the top of `src/qemu_jit.rs`
  lists `wasi_jit_call` and `wasi_jit_drop` as the interface; the comment
  at the `add_net_to_linker` call in `src/wasm.rs` mentions only
  `host_net_send` and `host_net_recv`; the `Stack.debug` comment in
  `src/qemu_net.rs` says `QEMU_NET_DEBUG=1`.
- Unused imports and fields produce compiler warnings (`cargo build`
  lists them).
