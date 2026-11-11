# The browser UI: windows, tabs and the programs they run

**What this covers.** How m-browser's GUI is put together: startup, the
egui user interface, tabs and their history, how pages are fetched and
shown, the program a `.wasm` page runs in its tab (QEMU, a wasi-gfx
component, or another core module shown as a text page), how that program
pauses while its tab is in the background, web tabs (Chromium in QEMU,
started from one saved web checkpoint), the session kept between runs,
and the exact path from typing the address of `qemu-system-x86_64.wasm`
to a running QEMU and back. Read
it before changing `src/main.rs`, `src/app.rs`, `src/tab_program.rs`,
`src/egui_tools.rs` or `src/winit_wasi.rs`. Rust and GUI terms are
explained in [02-concepts.md](02-concepts.md).

## Overview

```text
main()                                   src/main.rs
 |-- "--qemu-headless", "--wasm-headless" --> no window, see
 |                                            11-headless-and-automation.md
 `-- otherwise: winit EventLoop (ControlFlow::Poll) running App
       resumed()        create the main window "M", wgpu surface, egui
       window_event()   for every event of every window:
         1. egui sees the main window's events  (EguiRenderer::handle_input)
         2. input for the current tab's program (TabProgram::handle_event)
         3. RedrawRequested of the main window -> handle_redraw():
            draw_ui() builds the UI and queues Actions;
            tick_programs() reads what the tabs' programs reported
         4. act() carries out the queued Actions
```

| File | Contents |
|---|---|
| `src/main.rs` | Declares the modules; GUI or headless mode |
| `src/app.rs` | `App` (UI state, winit's `ApplicationHandler`), `Tab`, `Action`, the UI (`draw_ui`), navigation, child windows, web tabs, the session file |
| `src/tab_program.rs` | `TabProgram`: starting, pausing, resuming and stopping a tab's program; `module_kind()`; QEMU's arguments, disk lock, web setup copy and typing into Chromium |
| `src/egui_tools.rs` | `EguiRenderer`: glue between egui, egui-winit and egui-wgpu |
| `src/winit_wasi.rs` | `WinitEventToSurfaceProxy` (input for wasi-gfx components), `MyWindowWrapper` (a window as a wasi-gfx display), leftovers |
| `src/wasm.rs` | `Wasm` and `ComponentControl` (components), `run_wasm_module()` (QEMU, see [06-qemu-runner.md](06-qemu-runner.md)) |
| `src/wasm_headless.rs` | Other core modules, and `OutputPage`, their text page |

## Startup

1. `main()` runs `run()` with `pollster::block_on()` unless the first
   argument is `--qemu-headless` or `--wasm-headless`.
2. `run()` creates the winit `EventLoop` with `ControlFlow::Poll` (the
   loop never waits for events), creates the `App` and runs the loop with
   it (`run_app()`). `App::new()` makes a wgpu `Instance` and one tab
   (`Tab::new()`) whose address is the project README on
   raw.githubusercontent.com (`START_PAGE`), unless the session file of
   the last run brings its tabs back ([The session](#the-session)).
3. winit calls `resumed()`: it creates the main window "M", and
   `set_window()` asks for 3840x2160 physical pixels and builds `AppState`
   (wgpu adapter, device, queue, and the configuration of the window's
   *swap chain*, the images wgpu draws into and shows in turn: format
   `Bgra8UnormSrgb`, `PresentMode::AutoVsync`; plus the `EguiRenderer`).
4. Every `RedrawRequested` of the main window draws the UI and requests
   the next redraw: the UI is redrawn continuously, paced by the
   display's refresh rate.

**Note:** without a session file nothing is fetched at startup. The
address bar shows the README address, but the page stays empty (tab "New
Tab", status "Press Enter or Go to load the page") until you press Enter
or Go. With one, only the focused tab is loaded.

**Pitfall:** `AppState::new()` panics ("failed to select proper surface
texture format!") if the GPU surface does not offer `Bgra8UnormSrgb`.

## The UI

```text
+----------------------------------------------------------------------+
| [menu] [<-] [->] [ address bar ...............................] [>>] |
+------------+---------------------------------------------------------+
| [x] (tab)  |  page: Markdown, a text page, or the child window of    |
| [x] (tab)  |  the tab's program                                      |
| [+]        |                                                         |
+------------+---------------------------------------------------------+
| Status: QEMU is running                                              |
+----------------------------------------------------------------------+
```

egui is an *immediate-mode* UI: `handle_redraw()` calls `draw_ui()`,
which describes every panel again on each frame.
`TopBottomPanel::top("top_panel")` holds the menu (theme buttons,
"Reload", "Set up web tabs", "Save web checkpoint", "Quit"), back,
forward, the address bar
(`TextEdit::singleline(&mut App::current_location)`) and Go. The bottom
panel shows the current tab's status (`Tab::status_text()`: what its
program says, else the tab's own). `SidePanel::left` lists the tabs, each
with a close button, and "+", in a fixed width (`TAB_PANEL_WIDTH`, 180
logical pixels), so that the page area does not move with the labels.
`CentralPanel` shows the page. What the three panels leave
(`Context::available_rect()`) is the *page area*: `draw_ui()` returns it,
and `App::content` keeps it in whole physical pixels (egui's points times
`pixels_per_point()`; physical, because a new child window may not know
its scale factor yet). After each frame `TabProgram::place()` puts the
current tab's child window there, moving or resizing it only when the
area changed, for example when the main window is resized.

**Actions.** Creating a window and exiting need winit's
`ActiveEventLoop`, which `window_event()` receives and `handle_redraw()`
does not. So the UI only queues an `Action` (`Go`, `Back`, `Forward`,
`Reload`, `NewTab`, `SwitchTo`, `Close`, `SetUpWeb`, `SaveWebCheckpoint`,
`OpenCurrent`, `Quit`) in `App::actions`, and `window_event()` carries
the queue out with `App::act()` right after drawing.

## Tabs

A `Tab` holds its page, its history and the program its page runs;
`App::current_tab` is the `identifier` of the active one.

| Field | Meaning |
|---|---|
| `id` | 12 hex digits from `saved_state::new_owner_id()`. The saved states and disk overlays of the tab's programs end with it ([15-saved-states.md](15-saved-states.md)) |
| `identifier` | A number telling tabs apart in the UI |
| `label`, `location` | The label in the list, the address |
| `contents` | Markdown text, or for a `.wasm` address the module's path |
| `status` | The status bar's text while no program speaks for the tab |
| `back`, `forward` | The history |
| `program` | `Option<TabProgram>`: what a `.wasm` or web page runs (see below) |
| `loaded` | False for a tab the session brought back, until it is opened |
| `saved_state` | The saved state its program resumes or resumed from |
| `scroll`, `restore_scroll` | The Markdown page's scroll position as last drawn, and one to restore (from the session) |

| You | What happens |
|---|---|
| Enter or Go | `App::load()`: the tab's program stops; the old address goes onto `back` (unless it is the same) and `forward` is cleared; the page is loaded, and a `.wasm` or web page's program started. In a web tab, a web address is only typed into its Chromium |
| Back, Forward | Move between the stacks and `load()` without changing them |
| Menu: Reload | `load()` the current address again: a program starts afresh |
| A tab's label | `switch_to()`: the current tab's program pauses and its window hides; the tab clicked shows its window and goes on. Nothing is loaded again, except a tab the session brought back, which loads now |
| A tab's close button | `close()`: its program stops; closing the current tab shows its neighbour; closing the last one leaves a new, empty tab |
| "+" | `new_tab()`: the current tab's program pauses; the new tab has the README address and an empty page |
| Menu: Set up web tabs, Save web checkpoint | See [Web tabs](#web-tabs) |
| Menu: Quit, closing the window | `shutdown()`: programs that can be saved save themselves, every program stops (the QEMUs get up to 5 s to close their disk images), and the session file is written |

`get_heading()` gives the label and the window title ("M - <label>"): the
first Markdown heading, else the last part of the address.

**Scroll positions.** Each page of each tab keeps its scroll position:
`show_markdown()` gives its `ScrollArea` an id made of the tab and the
address (`id_salt`), and egui remembers scroll offsets per id. Going back
to a page in a tab finds it scrolled where you left it, too. The session
keeps the position of each tab's page (`Tab::scroll`).

**Kinds of addresses** (`page_kind()`):

| Address | Kind | Shown as |
|---|---|---|
| ends in `.wasm` | `Wasm` | its program, in the tab ([A tab's program](#a-tabs-program)) |
| ends in `.wasmsavestate` | `SavedState` | that saved state, resumed ([The session](#the-session)) |
| `about:web-setup` (`WEB_SETUP`) | `WebSetup` | the web setup ([Web tabs](#web-tabs)) |
| a `file://` path, or a path ending in `.md` or `.markdown` (before `?` or `#`) | `Markdown` | the Markdown page |
| anything else, such as `example.com` or `https://example.com/` | `Web` | Chromium in QEMU ([Web tabs](#web-tabs)) |

**Markdown pages.** egui_commonmark's `CommonMarkViewer` renders the page
text; HTML is for web tabs, so an HTML file opened with `file://` shows as
text. On
every frame the page is also parsed with pulldown_cmark to register each
link URL with `CommonMarkCache::add_link_hook()`; after drawing,
`get_link_hook(&url) == Some(true)` means it was clicked, and m-browser
navigates itself instead of opening the system browser.

**Pitfall:** link URLs are used as written. A relative link such as
`SECOND_PAGE.md` is not resolved against the current page and fails
("Failed to load page"); use absolute URLs or `file://` paths.

**Pitfall:** pages are fetched with `reqwest::blocking::get()` on the UI
thread, so the window freezes until the server answers (for a remote
`.wasm`, until the file is downloaded). HTTP status codes are not checked
for Markdown pages: a 404 page's body is shown as the page.

## Navigation functions

| Function | What it does |
|---|---|
| `App::load(address, add_to_history)` | Stops the tab's program (a web tab given a web address keeps it) and updates its history; then by `page_kind()`: `.wasm`: `fetch_wasm()`, then `start_program()`; Markdown: `navigate()`; web: `start_qemu_tab()` from the web checkpoint; a saved state: `start_saved()` |
| `page_kind(address)`, `web_label(address)` | The kind of page an address is (table above); a web tab's label, the address's host |
| `navigate(location)` | `file://`: reads the file. Anything else: HTTP GET. Returns the text, or "Failed to load page" (with the reason, for a file) |
| `fetch_wasm(url)` | `file://`: the file's absolute path. Otherwise downloads the module into the working directory as `downloaded-<hash>.wasm` (the first 12 hex digits of its SHA-256) and returns that path; HTTP errors fail |
| `start_program(...)` | `module_kind()`, a child window if the kind draws (`spawn_child_window()`), then `TabProgram::start()` |
| `start_qemu_tab(...)`, `start_saved(...)` | QEMU in a new, hidden child window (`QemuStart`: a web tab, the web setup); a saved state of either engine |
| `is_wasm(s)` | `s.ends_with(".wasm")`: an address with `?query` after `.wasm` is not a wasm page |
| `is_local_file(s)` | `s.starts_with("file://")`; the rest is the path (`file:///home/me/a.md` is `/home/me/a.md`) |

**Note:** downloads are named after their content so that each tab's
module stays as it was while other tabs download theirs: a saved state
names its module and resumes only with the very same file. They are never
deleted; `.gitignore` lists `downloaded-*.wasm`.

## A tab's program

`module_kind()` (`src/tab_program.rs`) reads the module's import and
export sections with wasmparser, which come before its code:

| Kind | Recognized by | Runs with | Shows |
|---|---|---|---|
| QEMU | a core module importing `env::wasi_jit_*` or `env::host_gfx_*`, or exporting `wasi_qemu_*` | `run_wasm_module()` | its child window |
| Component | the component encoding (version bytes `0d 00 01 00`) | `Wasm::run_wasm()` | its child window |
| Core | any other core module | `wasm_headless::start()` | a text page |

`TabProgram` is an enum of `QemuTab`, `ComponentTab`, `CoreTab` and
`Failed(message)`, a page that cannot run (the central panel says why).
All kinds answer the same calls:

| Call | QEMU | Component | Other core module |
|---|---|---|---|
| `pause()`: the tab went to the background | `Request::Pause`, QEMU's monitor command `stop`; window hidden | `ComponentControl::pause()`; no more frames; window hidden | `Command::Pause`: it waits at its next save point |
| `resume()`: the tab is back | window shown; `Request::Continue` (`cont`); a repaint | window shown; `ComponentControl::resume()` | `Command::Continue` |
| `stop()`: another page, the tab closed, Quit | the stop flag: QEMU quits | `ComponentControl::stop()` | `Command::Quit` |
| `handle_event()` | its input queue | the surface proxy | (none) |
| `status()` | "QEMU is running", "QEMU is paused", "QEMU has exited", errors | "running", "paused", ... | "running", "exited with status 0", ... |

**Why a child window per program.** QEMU draws with softbuffer and
components with wgpu, which cannot share a window. And a program in the
background keeps its window, hidden (`set_visible(false)`), so it never
has to set up its drawing again. A separate native window also lets a
program draw at its own pace from its own thread, while egui draws the
browser on the UI thread, with no copying between the two.

**Where the window goes.**

| Kind | Window | Why |
|---|---|---|
| QEMU | Covers the page area and follows its size | QEMU's display scales the guest's screen to fit the window, keeping its proportions, black around it, and maps the pointer back ([09-display-and-input.md](09-display-and-input.md#pitfalls)) |
| Component | 1080x720 logical pixels (`COMPONENT_SIZE`), or the page area if smaller, at its top-left corner; follows its position, never changes size | wasi-webgpu sets up a component's swap chain once, at the size its window has when the component connects its graphics context (`display_api_ready()`), and never again. A window resized later would keep a part the component never paints. The page panel's own background shows around the window |

**Pitfall:** X11 does not clear a window that appears: until its program
paints it, it shows whatever the screen showed there, such as the page
of the tab before. So a program must paint all of its window. QEMU
paints every pixel of it, from the start of its display on; its window
is created hidden and appears only once QEMU runs
(`QemuTab::update_visibility()`), so a tab waiting for the disk image,
or one whose QEMU failed, shows its status in the page area instead.

### Pausing, kind by kind

**QEMU.** `stop` halts the guest's processors and its clocks. QEMU's main
loop goes on (it serves the monitor and refreshes the display) but idles:
a paused QEMU used no measurable CPU time when this was measured with the
headless runner. The pause and continue requests reach QEMU between two
main-loop steps (`Saver::between_steps()`, see
[15-saved-states.md](15-saved-states.md)). While hidden, the window loses
its picture (X11 keeps no contents for an unmapped window), and QEMU
sends an unchanged screen only about once a second, so `resume()` also
asks for a repaint: `qemu_gfx::request_repaint()` queues an
`EVENT_REPAINT`, which `host_gfx_poll_events()` takes at QEMU's next
display refresh (about 30 a second) to present the last frame again
(`QemuGfxState::last_frame`), with the window's size in case the main
window was resized meanwhile. Neither reaches the guest.

**Note:** the guest's own clock falls behind by the time it was paused,
until the guest corrects it (NTP, for example).

**Components.** `ComponentControl` (`src/wasm.rs`) gives each component
an engine of its own with *epoch interruption*: incrementing the
engine's epoch makes the component's code call a function of
m-browser's at its next check (wasmtime checks on entering a function
and at the end of each loop iteration). `pause()` increments it, and
that function waits while paused; `stop()` increments it, and the
function makes the component trap. Most drawing programs spend their time
waiting in the host for their next animation frame instead; the frame
thread sends none while paused, so they simply wait. The component's
monotonic clock is a `ProgramClock` that stands still while it is paused,
so an animation goes on where it stopped instead of jumping ahead.

**Pitfall:** wasi-gfx-runtime panics when it sends an event to a
component whose event channel is full (five events; key and button
events) or closed (the component has ended), and on any frame for an
ended component. So only the current tab's program gets events, a paused
component gets no frames, the frame thread stops when its component ends,
and both calls sit in `catch_unwind` (`ComponentTab::handle_event()`).

**Other core modules.** These are WASI commands such as
`src/saved_state/testdata/ticker.wat`, run exactly as by
`m-browser --wasm-headless` ([15-saved-states.md](15-saved-states.md)),
with their standard output and error on a text page instead of the
terminal. An `OutputPage` keeps about the last 1 MiB (`OUTPUT_LIMIT`),
dropping the oldest lines; `show_output()` shows it in a monospace
scroll area that follows its end. Such a program gets no directories,
environment, arguments (argv[0] is its file name) or standard input. It
pauses at its next *save point*: its next call to one of the WASI
functions that m-browser wraps (writing, reading, sleeping, the clock).

**Pitfall:** a program that only computes, calling no WASI function,
never reaches a save point and runs on while its tab is in the
background. And a reactor (no `_start`) does not run in a tab: the tab
says so; run it with `--wasm-headless --step EXPORT`.

## Web tabs

An address that is neither a `.wasm` module nor Markdown is a web page:
m-browser shows it with Chromium, running in QEMU in the tab. Booting the
guest until Chromium waits for an address takes about five minutes, so it
is done once: that moment is saved as the *web checkpoint*, and every web
tab starts its own QEMU from it in a few seconds.

```text
once  (menu: Set up web tabs, then Save web checkpoint)
  m.qcow2 --copy--> m-web-<UTC>.qcow2 --boot--> Chromium idle --> checkpoint
                    (frozen once saved)                         (about 0.9 GB)
each web tab
  checkpoint --resume, about 2 s--> QEMU writing an overlay of its own
             --> Ctrl+L, the address and Enter typed into Chromium
```

**Making the web checkpoint.**

1. Menu: **Set up web tabs** (`Action::SetUpWeb`) opens a tab at
   `about:web-setup`. Its QEMU (`QemuStart::Setup`) first copies
   `m.qcow2` to `m-web-<UTC time>.qcow2` (`copy_disk()`), holding
   `m.qcow2`'s lock so that no QEMU writes it meanwhile, then boots the
   copy. Chromium starts by itself (qemu-wasi's docs/16-guest-image.md).
2. When Chromium is up, idle and waiting for an address, menu: **Save web
   checkpoint** (`Action::SaveWebCheckpoint`, enabled only in the setup
   tab). Its QEMU saves a checkpoint (`Request::Save`, see
   [15-saved-states.md](15-saved-states.md#qemu-checkpoints)): the copy
   is frozen as the checkpoint's disk, and QEMU goes on writing an
   overlay. When QEMU reports it written (`Report::Saved`, which
   `App::tick_programs()` reads on every frame), it becomes
   `App::web_checkpoint`, and the session file is written at once.
3. Close the setup tab.

A new web checkpoint replaces the old one, whose file and disk images
are deleted (`retire_web_checkpoint()`), except images the new one uses
too: a setup that saved twice froze its first overlay into the second
checkpoint, on top of the copy. Only m-browser's own web disks are ever
deleted (`is_web_disk()`: never `m.qcow2`). Web tabs still running on
deleted images go on: Linux deletes an open file's data only when it is
closed.

**Pitfall:** the copy needs `m.qcow2` to be free: close a tab that runs
`qemu-system-x86_64.wasm` first, or the setup waits 12 s and gives up.

**A web tab.** `App::load()` with a web address starts
`QemuStart::Resume { checkpoint, url }` in a hidden window
(`start_qemu_tab()`). The QEMU thread checks the checkpoint
(`prepare_resume()`), starts QEMU with its saved arguments, gives its disk
an overlay of its own and loads the state. When the Saver reports
`Report::Resumed`, `QemuTab::tick()` shows the window and types the
address (`QemuTab::type_url()`): Ctrl+L, which moves the keyboard to
Chromium's address bar and selects what is there, then the address and
Enter, from `qemu_keymap::chord_events()` and `text_events()`. QEMU's
display delivers them six per refresh. In a web tab that already runs, a
new address is only typed (`TabProgram::open_url()`); back and forward
type the earlier address again. Without a web checkpoint, a web address
shows a page explaining how to make one (`NO_WEB_CHECKPOINT`).

| Step | Time on the development machine |
|---|---|
| Copying `m.qcow2` | about 2 s |
| Booting the copy until Chromium is idle | about 5 to 6 minutes |
| Saving the web checkpoint | a few seconds; about 0.9 GB |
| A web tab: resuming | about 2 s |
| Its first page drawn | up to a minute: a resumed QEMU starts with no translated code and translates Chromium's again |

**Disk files.** Each web tab's QEMU writes an overlay,
`qemu-system-x86_64-<UTC>-0-<tab id>.disk0.qcow2`, which grows with
Chromium's cache. Nothing needs it once the tab's QEMU stops, since a web
tab comes back by its address, so `App::retire()` deletes it, and the
setup tab's overlays too (`remove_overlays()`, which only matches names
ending with the tab's own id and keeps the images the web checkpoint is
made of). A setup that saved no checkpoint also deletes its copy
(`QemuTab::stop()`).

**Note:** the address is typed as on a US keyboard (`char_to_evdev()`);
characters it has no key for are left out.

## The session

`m-browser-session.json` in the working directory (`SESSION_FILE`) keeps
the browser's tabs between runs: their ids, labels, addresses, history,
scroll positions and saved states, the focused tab, and the web
checkpoint. It is JSON (serde_json), readable and editable by hand:

```json
{
  "version": 1,
  "current": "8c1f04a2b7de",
  "web_checkpoint": "/home/me/m-browser/qemu-system-x86_64-20261110T120000Z-1-3a4b5c6d7e8f.wasmsavestate",
  "tabs": [
    {
      "id": "8c1f04a2b7de",
      "label": "example.com",
      "location": "example.com",
      "back": [],
      "forward": [],
      "scroll": 0.0,
      "saved_state": null
    }
  ]
}
```

**Quitting** (`App::shutdown()`):

1. `save_programs()`: the programs that can be saved save themselves
   (`TabProgram::save_for_session()`), within 20 s
   (`SESSION_SAVE_TIMEOUT`). A tab's new saved state replaces its old one,
   which is deleted (`remove_own_state()` only deletes files whose names
   end with the tab's id).
2. Every program stops; the QEMUs get up to 5 s to close their disks.
3. `write_session()`, leaving out the web setup tab.

| Tab | Kept as | Comes back |
|---|---|---|
| Markdown page | address, scroll position | fetched again, scrolled where it was |
| Web page | address | its own QEMU from the web checkpoint, which opens the address again: the page reloads, and its state (scroll, forms) is gone |
| Core module with save points (a reactor, or a command transformed with Asyncify) | its hibernated state, `<module>-<UTC>-<n>-<tab id>.wasmsavestate` | resumed where it was |
| Other core module, component | address | started afresh |
| `qemu-system-x86_64.wasm`, on `m.qcow2` | address | boots afresh: a checkpoint would freeze `m.qcow2`, and its next boot would break that checkpoint |
| Web setup | not kept | |

**Note:** web tabs come back by address because a checkpoint of a
running Chromium guest takes about 0.9 GB; one per web tab would soon
fill the disk.

**Starting** (`App::new()` and `restore()`): every tab comes back as a
pointer (address, history, saved state; `Tab::loaded` false, status "not
loaded yet"). Only the focused tab is loaded, once the first frame has
measured the page area (`Action::OpenCurrent`, `open_current()`): its
saved state is resumed
(`start_saved()`), else its address is loaded. Any other tab loads when
it is opened (`switch_to()`). A long session therefore starts as fast as
a short one, and takes memory only for the tabs in use.

**Saved states as pages.** A `.wasmsavestate` address resumes that state
in the tab (`PageKind::SavedState`): a hibernated core module, or a QEMU
checkpoint, which then runs as a web tab.

## Input

- Keys: a child window never takes the keyboard focus, so winit reports
  every key against the main window. Keys go to the current tab's
  program while egui does not want them (`wants_keyboard_input()`): while
  no text field such as the address bar has the focus.
- The pointer: events of the program's own window only.
- egui sees only the main window's events. A child window's pointer
  events are at the child's coordinates: egui would take them for events
  at the same place of the main window, where a click could hit the
  address bar or a tab. A click into a program's window takes the focus
  away from egui's text field (`surrender_focus`), so typing goes to the
  program.

## From the address bar to a running QEMU

```text
UI thread                                      QEMU thread
act(Go) -> load(): old program stop()  - - - > old QEMU quits on its own
fetch_wasm(): the file:// path
start_program(): module_kind() = Qemu,
  spawn_child_window(), wasi-gfx Surface
QemuTab::start(): queue, QemuControl,  ----->  lock_disk_image("m.qcow2"),
                  stop flag                      waiting up to 12 s
                                               run_wasm_module(path, args, ...,
                                                 Saving { owner: tab id })
keys, mouse -> QemuTab::queue  ------------->  host_gfx_poll_events()
child window  <------------------------------  host_gfx_update_framebuffer()
```

1. You enter `file:///home/me/src/m-browser/qemu-system-x86_64.wasm`.
   The UI queues `Action::Go`; `App::act()` calls `load()`, which stops
   the tab's old program and gets the module's path from `fetch_wasm()`.
2. `start_program()` asks `module_kind()`, which finds QEMU's imports,
   creates a hidden child window over the page area, and calls
   `TabProgram::start()`.
3. `QemuTab::start()` wraps the window as a wasi-gfx `Surface`, creates
   the input queue (`qemu_gfx::new_event_queue()`) with the window's size
   in it, a `QemuControl` for pause and continue, and a stop flag
   (`Arc<AtomicBool>`), and starts the QEMU thread.
4. The QEMU thread takes the disk lock, sets `started` (the window
   appears at the next frame), builds the arguments and calls
   `crate::wasm::run_wasm_module(&path, &args, &preopen_dirs,
   Some(surface), queue, Some(stop), Some(saving))`, where `saving`
   carries the control and the tab's id.
5. `run_wasm_module()` loads QEMU (the first run compiles it, which takes
   a while, and caches the result), adds `-smp` for the multi-threaded
   build, sets up WASI, and runs QEMU's main loop until QEMU exits or the
   flag is set ([06-qemu-runner.md](06-qemu-runner.md)).

**The disk image lock.** `QEMU_DISK_IMAGE` (`"m.qcow2"`, in
`src/tab_program.rs`) is relative to the working directory; QEMU sees it
as `/data/m.qcow2`. Two QEMUs writing one qcow2 image (QEMU's
copy-on-write disk format) corrupt it, and QEMU on WASI cannot lock files
itself, so `lock_disk_image()` takes an exclusive *advisory* lock
(`File::try_lock()`: only programs that also ask for the lock see it),
held by `_image_lock` until the thread ends:

| Situation | Result |
|---|---|
| Image missing or not openable | No lock; QEMU starts and reports the error itself |
| Locked by another QEMU | Waits up to 12 s (`DISK_WAIT`), long enough for a QEMU that is quitting (another page in the same tab, a closed tab). Then: "m.qcow2 is in use by another QEMU (in another tab or another m-browser); not starting a second one on the same disk image" in the status bar; the child window stays empty |
| The tab's program is stopped meanwhile | QEMU never starts |
| Locking not supported | "Cannot lock m.qcow2 (...); starting QEMU anyway" |

**Note:** so for now only one tab at a time can run QEMU on `m.qcow2`.

**Pitfall:** the headless runner does not take this lock. Never run it on
the image a GUI QEMU is using.

**QEMU's arguments** (`args` in `QemuTab::start()`):

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

## Stopping programs

| Who | What |
|---|---|
| `App::retire()` | `TabProgram::stop()`, without waiting; keeps a QEMU's thread in `App::stopping`. Used by `load()` (another page in the tab) and `close()`. A web tab's overlay is deleted ([Web tabs](#web-tabs)) |
| `App::shutdown()` | Saves what can be saved, stops every tab's program, waits up to 5 s in all for the QEMU threads, and writes the session file ([The session](#the-session)). Used by Quit and when the window is closed, before `event_loop.exit()` |
| `run_qemu_reactor()` (`src/wasm.rs`) | Between two main-loop steps it sees QEMU's stop flag and calls QEMU's export `wasi_qemu_quit()`: QEMU powers the guest off (for the guest, a power cut), flushes and closes the disk images, also while paused. It keeps stepping until QEMU returns, for at most `QUIT_TIMEOUT` (10 s) |

If a program ends by itself (the guest powered off, a command finished),
its status says so ("QEMU has exited", "exited with status 0"), and its
window or page stays until you navigate or close the tab.

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

**Note:** `RedrawRequested` and `Resized` count for the main window only.
Before tabs owned their programs, a child window's redraw requests also
redrew the main window, and its `Resized` event resized the main
window's swap chain to the child's size.

## Leftovers you can ignore

| Item | Status |
|---|---|
| `WasiWinitEventLoop`, `create_wasi_winit_event_loop()`, `WasiWinitEventLoopProxy`, `MainThreadAction` (`src/winit_wasi.rs`) | Never called: an alternative design in which wasm threads ask the main thread to create windows |
| `QemuGfxState::queue_event()`, `AppState::scale_factor` (always 1.0) | Unused |
| `set_window()` | Passes the width twice to `AppState::new()`; the first `Resized` event fixes the surface size |

## How to ...

**Change QEMU's arguments.**

1. Edit `args` in `QemuTab::start()` in `src/tab_program.rs`.
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

1. Change `QEMU_DISK_IMAGE` in `src/tab_program.rs`.
2. Change `file=/data/m.qcow2` in the `-drive` argument to the same name:
   if they differ, the lock protects another file than QEMU opens. Change
   `format=qcow2` too if needed.
3. For an image outside the working directory, add a preopen for its
   directory, use that guest path, and lock the host path.
4. Add the new name to `.gitignore` (it lists `m.qcow2`), then rebuild.

**Add a menu item.**

1. Find `ui.menu_button(icons::ICON_MENU, ...)` in `draw_ui()` and add a
   button in its closure.
2. If the action changes tabs or needs the event loop (windows, exit),
   push an `Action` there; for a new kind of action, add an `Action`
   variant and handle it in `App::act()`. Work that takes time belongs
   on a thread of its own: anything slow on the UI thread freezes the
   window.
3. Rebuild and try it.

A "Close tab" item can reuse the action of the close buttons:

```rust
if ui.button("Close tab").clicked() {
    actions.push(Action::Close(current));
    ui.close_menu();
}
```
