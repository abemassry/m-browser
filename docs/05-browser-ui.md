# The browser UI: windows, pages and starting QEMU

**What this covers.** How m-browser's GUI is put together: startup, the
egui user interface, how pages are fetched and shown, the child window
that wasm apps draw into, and the exact path from typing the address of
`qemu-system-x86_64.wasm` to a running QEMU and back. Read it before
changing `src/main.rs`, `src/app.rs`, `src/egui_tools.rs`,
`src/winit_wasi.rs` or `src/fill.rs`. Rust and GUI terms are explained in
[02-concepts.md](02-concepts.md).

## Overview

```text
main()                                    src/main.rs
 |-- first argument "--qemu-headless" --> headless::run(): no window, see
 |                                        11-headless-and-automation.md
 `-- otherwise: winit EventLoop (ControlFlow::Poll) running App
       resumed()        create the main window "M", wgpu surface, egui
       window_event()   for every event of every window:
         1. egui sees the event        (EguiRenderer::handle_input)
         2. input for the running app  (QEMU's queue / wasi-gfx proxy)
         3. act on flags the UI set    (quit, spawn / close child window)
         4. RedrawRequested            -> handle_redraw(): build + draw UI
```

| File | Contents |
|---|---|
| `src/main.rs` | Declares the modules; GUI or headless mode |
| `src/app.rs` | `App` (all UI state, winit's `ApplicationHandler`), the UI, navigation, the child window, starting and stopping QEMU |
| `src/egui_tools.rs` | `EguiRenderer`: glue between egui, egui-winit and egui-wgpu |
| `src/winit_wasi.rs` | `WinitEventToSurfaceProxy` (input for wasi-gfx components), `MyWindowWrapper` (a window as a wasi-gfx display), leftovers |
| `src/wasm.rs` | `Wasm` (component runtime), `run_wasm_module()` (QEMU, see [06-qemu-runner.md](06-qemu-runner.md)) |

## Startup

1. `main()` runs `run()` with `pollster::block_on()` unless the first
   argument is `--qemu-headless`.
2. `run()` creates the winit `EventLoop` with `ControlFlow::Poll` (the
   loop never waits for events), creates the `App` and runs the loop with
   it (`run_app()`). `App::new()` makes a wgpu `Instance`, one tab whose
   address is the project README on raw.githubusercontent.com, channels
   and a `Wasm` runtime for components.
3. winit calls `resumed()`: it creates the main window "M", and
   `set_window()` asks for 3840x2160 physical pixels and builds `AppState`
   (wgpu adapter, device, queue, and the configuration of the window's
   *swap chain*, the images wgpu draws into and shows in turn: format
   `Bgra8UnormSrgb`, `PresentMode::AutoVsync`; plus the `EguiRenderer`).
4. Every `RedrawRequested` draws the UI and requests the next redraw: the
   UI is redrawn continuously, paced by the display's refresh rate.

**Note:** nothing is fetched at startup. The address bar shows the README
address, but the page stays empty (status "Loading...", first tab without
a label) until you press Enter or Go.

**Pitfall:** `AppState::new()` panics ("failed to select proper surface
texture format!") if the GPU surface does not offer `Bgra8UnormSrgb`.

## The UI

```text
+----------------------------------------------------------------------+
| [menu] [<-] [->] [ address bar ...............................] [>>] |
+------------+---------------------------------------------------------+
| (tab)      |  page: Markdown (CentralPanel), or the child window     |
| [+]        |  while a .wasm page is open                             |
+------------+---------------------------------------------------------+
| Status: Loaded                                                       |
+----------------------------------------------------------------------+
```

egui is an *immediate-mode* UI: `handle_redraw()` describes every panel
again on each frame. `TopBottomPanel::top("top_panel")` holds the menu
(theme buttons, "Quit", "spawn child window (M)"), back, forward, the
address bar (`TextEdit::singleline(&mut self.current_location)`) and Go;
the bottom panel the status; `SidePanel::left` the tabs and "+";
`CentralPanel` the page, drawn only while there is no child window.

Creating a window and exiting need winit's `ActiveEventLoop`, which
`window_event()` receives and `handle_redraw()` does not. So the UI only
sets a flag (`quit_pressed`, `spawn_child_window`, and likewise
`close_child_window`), and `window_event()` acts on it while handling the
next event, which comes at once since redraws are continuous.

**Tabs and history.** A `Tab` has a `label`, `location`, `contents`, an
`identifier` and the stacks `back` and `forward`; `App::current_tab` names
the active one. Enter or Go on a Markdown address pushes the old address
onto `back` and clears `forward`; the arrows move between the stacks and
reload with `go()`; clicking a tab reloads it; "+" adds a tab with the
README address and an empty page. `get_heading()` gives the label and the
window title ("M - <label>"): the first Markdown heading, else the last
part of the address. Typing a `.wasm` address leaves the history alone;
clicking a link updates it.

**Pages are Markdown only.** egui_commonmark's `CommonMarkViewer` renders
the page text; there is no HTML engine, so an HTML page shows as text. On
every frame the page is also parsed with pulldown_cmark to register each
link URL with `CommonMarkCache::add_link_hook()`; after drawing,
`get_link_hook(&url) == Some(true)` means it was clicked, and m-browser
navigates itself instead of opening the system browser.

**Pitfall:** link URLs are used as written. A relative link such as
`SECOND_PAGE.md` is not resolved against the current page and fails
("Failed to load page"); use absolute URLs or `file://` paths.

**Pitfall:** pages are fetched with `reqwest::blocking::get()` on the UI
thread, so the window freezes until the server answers (for a remote
`.wasm`, until the file is downloaded). HTTP status codes are not checked:
a 404 page's body is shown as the page.

## Navigation functions

| Function | What it does |
|---|---|
| `go(url)` | Logs and calls `navigate(url)` |
| `navigate(location)` | `.wasm`: `download_wasm()`, returns a path. `file://`: reads the file. Anything else: HTTP GET. Returns the text, or "Failed to load page" |
| `download_wasm(url)` | `file://`: copies the file to `downloaded.wasm` in the working directory (unless it is that file) and returns the original path. Otherwise downloads to `downloaded.wasm` and returns that name |
| `is_wasm(s)` | `s.ends_with(".wasm")`: an address with `?query` after `.wasm` is not a wasm page |
| `is_local_file(s)` | `s.starts_with("file://")`; the rest is the path (`file:///home/me/a.md` is `/home/me/a.md`) |
| `back()`, `forward()` | Stubs returning `Ok(())`; the history code is inline in the buttons |

**Pitfall:** the local-file and download paths `unwrap()` their errors: a
missing or non-UTF-8 `file://` page or a failed `.wasm` download panics
and closes the browser.

## The child window

A wasm page draws into a second native window, created as a *child* of
the main window: it sits inside it at logical position (200, 27), right of
the side panel and below the top bar, measures 1080x720 logical pixels,
and moves with it. QEMU draws into it with softbuffer from its own thread
([09-display-and-input.md](09-display-and-input.md)); components draw with
wgpu through wasi-gfx. A separate native window lets the app draw at its
own pace from its own thread, while egui draws the browser on the UI
thread, with no copying between the two.

`spawn_child_window()` passes the main window's `raw_window_handle()` to
`with_parent_window()` (`unsafe`: winit cannot check the handle) and
creates the window, which `window_event()` then wraps as a wasi-gfx
surface: `wasi_surface_wasmtime::Surface::new(Box::new(MyWindowWrapper(
child)))`. `App::child_window` holds it and `App::child_window_id` its id
(the sentinel `2.into()` means "none"). Leaving the page hides it
(`set_visible(false)`) and resets both.

**Note:** according to winit's documentation, parent windows are not
supported on Wayland, so there the "child" is not embedded.

**Pitfall:** the child window keeps its size: it follows neither the main
window nor the guest's screen.

**Pitfall:** `EguiRenderer::handle_input()` gets the events of both
windows, so egui also sees the child window's pointer events, at the
child's coordinates, as if they happened in the main window. Read from the
code (not confirmed by testing): a click near the top-left of the guest
screen may also hit the egui widget at the same coordinates of the main
window, such as the address bar (which then takes the keyboard from QEMU)
or a tab button (which navigates away and stops QEMU). Checking
`window_id` before `handle_input()` would prevent it.

## From the address bar to a running QEMU

```text
UI thread: App::window_event()             QEMU thread
stop_qemu() on the old QEMU  - - - - - - > old QEMU quits on its own
child window + wasi-gfx Surface
header check: core module
qemu_event_queue = new_event_queue()
spawn  -------------------------------->  join the old QEMU's thread
App::qemu = QemuRun { stop, thread }      lock_disk_image("m.qcow2")
                                          run_wasm_module(path, args, ...)
keys, mouse -> qemu_event_queue  ------>  host_gfx_poll_events()
child window  <-------------------------  host_gfx_update_framebuffer()
```

1. You enter `file:///home/me/src/m-browser/qemu-system-x86_64.wasm`.
   `go()` -> `navigate()` -> `download_wasm()` copies it to
   `downloaded.wasm`, returns the original path, and the handler sets
   `spawn_child_window`.
2. On the next event `window_event()` stops a running QEMU with
   `stop_qemu()`; if a child window exists, it signals the component
   threads, hides the window, and replaces the channels and `Wasm`.
3. It creates the child window and its wasi-gfx `Surface`, and picks the
   file: the original for `file://` addresses, else `downloaded.wasm`.
4. It reads the file's first 8 bytes. Bytes 4-7 equal to `01 00 00 00`
   (core module, version 1) mean QEMU; anything else (`0d 00 01 00` is a
   component) or a read error means the component path. `[detect]` lines
   on stderr show the decision. Any core module is treated as QEMU.
5. It creates the input queue (`crate::qemu_gfx::new_event_queue()`, kept
   in `App::qemu_event_queue`) and a stop flag (`Arc<AtomicBool>`),
   starts the QEMU thread, and keeps both in `App::qemu` (`QemuRun`).
6. The QEMU thread waits for the QEMU it replaces (which must close the
   disk image first), takes the disk lock, builds the arguments and calls
   `crate::wasm::run_wasm_module(&wasm_path, &args, &preopen_dirs,
   Some(surface), gfx_event_queue, Some(stop))`.
7. `run_wasm_module()` loads QEMU (the first run compiles it, which takes
   a while, and caches the result), adds `-smp` for the multi-threaded
   build, sets up WASI, and runs QEMU's main loop until QEMU exits or the
   flag is set ([06-qemu-runner.md](06-qemu-runner.md)).

**The disk image lock.** `QEMU_DISK_IMAGE` (`"m.qcow2"`) is relative to
the working directory; QEMU sees it as `/data/m.qcow2`. Two QEMUs writing
one qcow2 image (QEMU's copy-on-write disk format) corrupt it, and QEMU on
WASI cannot lock files itself, so `lock_disk_image()` takes an exclusive
*advisory* lock (`File::try_lock()`: only programs that also ask for the
lock see it), held by `_image_lock` until the thread ends:

| Situation | Result |
|---|---|
| Image missing or not openable | No lock; QEMU starts and reports the error itself |
| Locked by another m-browser | "m.qcow2 is in use by another QEMU; not starting a second one on the same disk image"; QEMU does not start, the child window stays empty |
| Locking not supported | "Cannot lock m.qcow2 (...); starting QEMU anyway" |

**Pitfall:** the headless runner does not take this lock. Never run it on
the image a GUI QEMU is using.

**QEMU's arguments** (`args` in `window_event()`):

| Argument | Why |
|---|---|
| `qemu-system-x86_64` | Program name (`argv[0]`) |
| `-L /bios` | Firmware directory: `./pc-bios`, preopened as `/bios` |
| `-drive file=/data/m.qcow2,format=qcow2` | The disk image in the working directory |
| `-m 1024` | 1 GiB of guest RAM. With QEMU's default 128 MiB the desktop swaps constantly and X stalls long enough for the guest kernel to drop keystrokes; with 512 MiB, starting Chromium kept the kernel busy reclaiming memory. All of QEMU's memory must fit in wasm32's 4 GiB |
| `-accel tcg,tb-size=256` | TCG is QEMU's translator of guest code; this gives it a 256 MiB translation buffer. With 64 MiB the buffer filled up every few seconds while the desktop started, discarding all translated code; costs ~400 MiB more host memory |
| `-device qemu-xhci`, `-device usb-tablet` | A USB controller and a USB tablet, an absolute pointing device, so the guest pointer follows the host's; XHCI has no 1 kHz polling timer, unlike the older UHCI controller |
| `-monitor vc`, `-serial vc` | QEMU's monitor (its command prompt) and the guest's serial port as text consoles: Ctrl+Alt+2 and Ctrl+Alt+3; Ctrl+Alt+1 returns to the guest screen |
| `-nic user` | User-mode networking from `src/qemu_net.rs`. `QEMU_NIC` replaces the value, e.g. `user,hostfwd=tcp:127.0.0.1:2222-:22` ([10-networking.md](10-networking.md)) |

For the multi-threaded build `run_wasm_module()` appends `-smp N`, the
number of virtual CPUs (vCPUs), unless the arguments contain `-smp`
(`QEMU_SMP`; default 4 on hosts with at least 8 CPUs, else 2; at most 8).
It also passes every `WASI_*` and `QEMU_*` environment variable into
QEMU.

**Preopened directories** are `("pc-bios", "/bios")` and `(".", "/data")`,
with full permissions, relative to the working directory. So start the
browser from the m-browser directory (`./target/release/m-browser`).
Started elsewhere, opening `pc-bios` fails ("Error running QEMU core
module: ..." on stderr) or QEMU does not find `m.qcow2`.

## Stopping QEMU

| Who | What |
|---|---|
| `stop_qemu()` | Drops `qemu_event_queue` (no more input), takes `App::qemu`, sets the stop flag, returns the thread handle without waiting. Used when another page replaces the QEMU page |
| `shutdown_qemu()` | `stop_qemu()`, then waits up to 5 s for the thread. Used by "Quit" and when the window is closed, before `event_loop.exit()` |
| `run_qemu_reactor()` (`src/wasm.rs`) | Between two main-loop steps it sees the flag and calls QEMU's export `wasi_qemu_quit()`: QEMU powers the guest off (for the guest, a power cut), flushes and closes the disk images. It keeps stepping until QEMU returns, for at most `QUIT_TIMEOUT` (10 s) |

If QEMU ends by itself (the guest powered off, a bad option), its thread
finishes; the next `window_event()` sees `thread.is_finished()` and drops
`App::qemu` and the queue. The child window stays until you navigate.

**Pitfall:** the browser waits 5 s but QEMU may take up to 10 s. If it is
slower, the process exits while QEMU is still closing the disk image.

**Pitfall:** with the multi-threaded build, a thread QEMU created (a vCPU
thread, for example) that traps or calls `exit()` ends the whole
m-browser process (`thread_failed()` in `src/qemu_threads.rs`, see
[08-threads.md](08-threads.md)).

## Drawing the main window

`handle_redraw()` returns early while the window is minimized (printing
"Window is minimized" every frame), takes a texture from the swap chain
with `get_current_texture()`, builds the UI between
`EguiRenderer::begin_frame()` and `end_frame_and_draw()`, submits and
presents. Errors from `get_current_texture()`:

| Error | Handling |
|---|---|
| `Outdated` | Print "wgpu surface outdated", skip the frame (resizing, minimizing) |
| `Lost` | Reconfigure the surface, skip the frame |
| `Timeout`, anything else | Skip the frame; log `[gfx] no swap chain texture (...); skipping frame (N so far)` on the 1st, 2nd, 3rd, 5th, 9th, 17th, ... occurrence (when the previous count is 0 or a power of two) |

History: up to commit 3c091ad every error except `Outdated` reached
`expect("Failed to acquire next swap chain texture")`. When the host's
screen blanked or powered down, wgpu returned `Timeout`, the panic ended
the browser, and the VM inside died with it. Never `unwrap()` in the
redraw path: a panic there takes QEMU down too.

**Note:** the `RedrawRequested` branch does not check the window id, so
each redraw request of the child window also redraws the main window.

## The component (wasi-gfx) path in brief

1. `App::wasi_event_handler` (`WinitEventToSurfaceProxy`, built on
   `surface.proxy()`) forwards pointer, key and resize events. The call
   sits in `catch_unwind` because wasi-gfx-runtime panics when sending to
   a component that has exited. QEMU gets no proxy for this reason.
2. A thread locks `App::wasm_runtime` and runs `Wasm::run_wasm()`:
   component engine, Linker with wasi-webgpu, wasi-graphics-context,
   wasi-surface and WASI preview 2, then the component's `run`.
   `HostState::create_canvas()` hands over the child window's surface.
3. Another thread sends `animation_frame()` every 16 ms until
   `App::event_sender` signals.
4. To stop it, `App::sender` wakes the thread started by `Wasm::new()`,
   which advances the engine's epoch, so the component traps at its next
   epoch check; new channels and a new `Wasm` replace the old ones.

## Leftovers you can ignore

| Item | Status |
|---|---|
| `WasiWinitEventLoop`, `create_wasi_winit_event_loop()`, `WasiWinitEventLoopProxy`, `MainThreadAction` (`src/winit_wasi.rs`) | Never called: an alternative design in which wasm threads ask the main thread to create windows |
| `src/fill.rs` | Real code only with the Cargo feature `rwh_05`, which Cargo.toml does not define: `fill_window()`, called on every redraw, does nothing |
| `back()`, `forward()` | Stubs returning `Ok(())` |
| `Wasm::stop_wasm()`, `QemuGfxState::queue_event()`, `Tab::status`, `App::parent_window_id`, `AppState::scale_factor` (always 1.0) | Unused |
| "spawn child window (M)" menu item | Debug leftover: runs the current `file://` address, or else `downloaded.wasm`, in a new child window |
| `set_window()` | Passes the width twice to `AppState::new()`; the first `Resized` event fixes the surface size |

## How to ...

**Change QEMU's arguments.**

1. Edit `args` in the core-module branch of `App::window_event()`.
2. For a value computed at run time, create a `String` before the
   `vec![...]`, as `nic` is, and borrow it with `&`.
3. Use QEMU's paths (`/bios/...`, `/data/...`); a new host directory
   needs an entry in `preopen_dirs`. Leave out `-smp` unless you want to
   override the automatic value.
4. Try the arguments with the headless runner first
   ([11-headless-and-automation.md](11-headless-and-automation.md)).
5. Rebuild (`cargo build --release --offline`) and update the argument
   list in the qemu-wasi README.

**Change the disk image name.**

1. Change `QEMU_DISK_IMAGE` in `src/app.rs`.
2. Change `file=/data/m.qcow2` in the `-drive` argument to the same name:
   if they differ, the lock protects another file than QEMU opens. Change
   `format=qcow2` too if needed.
3. For an image outside the working directory, add a preopen for its
   directory, use that guest path, and lock the host path.
4. Add the new name to `.gitignore` (it lists `m.qcow2`), then rebuild.

**Add a menu item.**

1. Find `ui.menu_button(egui_material_icons::icons::ICON_MENU, ...)` in
   `handle_redraw()` and add a button in its closure.
2. If the action needs the event loop (windows, exit), only set a `bool`
   field there (declare it in `App`, initialise it in `App::new()`), and
   act on it in `window_event()` next to the `quit_pressed` check. Work
   that takes time belongs on a thread of its own: anything slow on the
   UI thread freezes the window.
3. Rebuild and try it.

A "Stop QEMU" item can reuse the flag that closes the child window, which
also stops QEMU:

```rust
if ui.button("Stop QEMU").clicked() {
    self.close_child_window = true;
}
```
