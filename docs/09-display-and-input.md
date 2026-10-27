# Display and input: QEMU's screen in a window, keys and mouse back

**What this covers.** How the emulated PC's screen gets from QEMU into
m-browser's window (or into a PNG file when headless), and how keyboard
and mouse events travel the other way, in m-browser (`src/qemu_gfx.rs`,
`src/qemu_keymap.rs`, the input code in `src/app.rs`) and, briefly, in
QEMU's display backend in the qemu-wasi repository (`display/`). Read it
before touching any of these, or when keys, clicks or the picture
misbehave. The QEMU side is described in more depth in the qemu-wasi
repository's docs/09-display-input-network.md.

## The two paths at a glance

```text
OUTPUT, on QEMU's main-loop thread, every ~30 ms
  guest draws into the emulated graphics card
   -> QEMU display layer: a DisplaySurface (BGRA pixels in QEMU's memory)
   -> display backend (qemu-wasi display/): copy what changed, zoom x2,
      host_gfx_update_framebuffer(ptr, len, width, height, stride)
   -> m-browser src/qemu_gfx.rs: BGRA bytes -> one 0x00RRGGBB u32 per pixel
        GUI:      softbuffer blits it into the child window
        headless: PNG written to $QEMU_SNAPSHOT (temp file + rename, <= 1/s)

INPUT
  winit events on the UI thread (App::window_event in src/app.rs)
   -> InputEvent { event_type, code, value } pushed on the GfxEventQueue
   -> QEMU's display refresh calls host_gfx_poll_events(buf, max)
   -> display backend: at most KEYS_PER_POLL key/text events per refresh
   -> QEMU input layer -> USB tablet, keyboard, or a text console
```

Terms used below: a *framebuffer* is an image of the screen in memory, here
4 bytes per pixel in the order blue, green, red, unused (*BGRA*).
*softbuffer* is a library that shows a pixel buffer kept in ordinary
memory in a window, without GPU code; to *blit* is to copy such a block
of pixels to the screen. A *console* is a screen QEMU can show: the
*graphic* console is the emulated graphics card's output; *text* consoles
are terminals QEMU draws itself, here for its monitor (a command prompt
for controlling QEMU) and for the guest's serial port. An *evdev code* is
the Linux kernel's number for a key, from `linux/input-event-codes.h`. A
*Store* is wasmtime's container for a running module and its host state
([02-concepts.md](02-concepts.md)).

| File | Role |
|---|---|
| `src/qemu_gfx.rs` | The four `host_gfx_*` host functions, `QemuGfxState`, `InputEvent`, `GfxEventQueue` |
| `src/qemu_keymap.rs` | `keycode_to_evdev()`: winit key code to evdev code |
| `src/app.rs`, `App::window_event()` | Turns winit events into `InputEvent`s |
| `src/headless.rs` | Synthetic typing and clicks ([11-headless-and-automation.md](11-headless-and-automation.md)) |
| qemu-wasi `display/wasi_gfx_dcl.c` | Hooks the backend into QEMU; refresh callback; console switching |
| qemu-wasi `display/wasi_gfx_display.c` | Framebuffer copy, zoom, input decoding, key pacing |

## Output: from QEMU's framebuffer to the window

On the QEMU side:

1. QEMU calls the backend's refresh callback, `wasi_gfx_dpy_refresh()`,
   from a main-loop timer every 30 ms (`GUI_REFRESH_INTERVAL_DEFAULT`).
2. The callback first polls input (below), then lets the graphics card
   redraw what the guest changed; changed rectangles are copied into the
   backend's own copy of the screen when they differ. Every 60th refresh
   (`FULL_REFRESH_EVERY`), or every one with `WASI_GFX_FULL_REFRESH=1`,
   the whole screen is redrawn and copied as a safety net.
3. `wasi_gfx_display_render()` sends a frame only if something changed or
   30 refreshes passed without one (`FRAME_RESEND_INTERVAL`, about a
   second), zooming it first ([Scaling](#scaling)).

On the host side, `host_gfx_update_framebuffer` in `src/qemu_gfx.rs`:

1. Reads the frame from QEMU's memory with `with_guest()` and converts
   each BGRA pixel into a `u32` `0x00RRGGBB` (softbuffer's format), row
   by row using `stride`.
2. Locks `QemuGfxState`.
3. GUI: resizes the softbuffer buffer if the size changed, copies the
   pixels in and calls `present()`. The frame appears 1:1 at the child
   window's top-left corner.
4. Headless (no softbuffer surface): if `QEMU_SNAPSHOT` is set and the
   last PNG is at least 1 s old, writes the frame as an RGB PNG to the
   path with its extension replaced by `png.tmp` (`screen.png` becomes
   `screen.png.tmp`), then renames it over the path, so a reader never
   sees half a file. Frames in between are dropped.

**Note:** frames are drawn from QEMU's thread. The UI thread never draws
into the child window (`fill::fill_window()` is a no-op, see
[05-browser-ui.md](05-browser-ui.md)). The time spent in this function
shows up as `[host: display]` in the profiler.

## Host functions

All four are imports from module `env`, registered by
`add_gfx_to_linker()`; [reference/host-imports.md](reference/host-imports.md)
lists every host import.

| Import | Signature | Semantics |
|---|---|---|
| `host_gfx_init` | `(width: i32, height: i32)` | Once, when the display starts, with the zoomed size. Creates the softbuffer `Context` and `Surface` on the child window's wasi-gfx surface and sizes the buffer. Without a surface (headless) logs "no wasi surface available (running headless?)" |
| `host_gfx_resize` | `(width: i32, height: i32)` | The guest's screen size or the zoom changed (zoomed size). Records it and resizes the buffer; the window is not resized |
| `host_gfx_update_framebuffer` | `(data_ptr: i32, data_len: i32, width: i32, height: i32, stride: i32)` | Show `height` rows of `width` BGRA pixels at `data_ptr` in QEMU's memory, rows `stride` bytes apart. Out of bounds: logged, frame dropped |
| `host_gfx_poll_events` | `(event_buf: i32, max_events: i32) -> i32` | Moves the queued input into `pending_events`, writes up to `max_events` events of 12 bytes (`type`, `code`, `value`, little-endian `i32`s) to `event_buf` and returns how many. The rest waits for the next call |

## The display state and who shares it

`QemuGfxState` (in `src/qemu_gfx.rs`):

| Field | Meaning |
|---|---|
| `width`, `height` | Current buffer size (the zoomed frame) |
| `sb_context`, `sb_surface` | softbuffer context and surface; `None` when headless |
| `wasi_surface` | `Arc` of the child window's wasi-gfx `Surface`, which softbuffer draws through |
| `pending_events` | Events taken from the queue that QEMU had no room for yet |
| `initialized` | Set by `host_gfx_init`, read nowhere |
| `snapshot_path` | `QEMU_SNAPSHOT`, read by `QemuGfxState::new()` |
| `last_snapshot` | When the last PNG was written (1 s throttle) |

`run_wasm_module()` puts it in an `Arc<Mutex<QemuGfxState>>`, the `gfx`
field of `QemuHostState`. The single-threaded build has one Store, and the
state moves along when QEMU is moved to a fresh Store. In the
multi-threaded build every thread's Store holds a clone of the same `Arc`
(`QemuThreads::new_store()`), so all threads see one display; in practice
only the main-loop thread calls these functions, since QEMU's display
refresh runs in its main loop.

The input queue is separate: `GfxEventQueue` is an
`Arc<Mutex<Vec<InputEvent>>>` that the UI thread (or the headless typist)
pushes to and `host_gfx_poll_events` drains; the closure in
`add_gfx_to_linker()` captures it. That function takes the display lock,
then the queue lock; the UI only ever takes the queue lock, so they cannot
deadlock.

## Input: from winit to QEMU

`App::window_event()` produces events only while QEMU runs (that is, while
`App::qemu_event_queue` exists):

- **Mouse**, only for events of the child window (`window_id ==
  child_window_id`). `CursorMoved` gives `(1, 0, x)` and `(1, 1, y)` in
  physical window pixels. `MouseInput` gives `(2, button, 1 or 0)` with
  left 0, middle 1, right 2, back 5, forward 6 (other buttons are
  dropped). Each `MouseWheel` event gives a press and a release of button
  3 (up) or 4 (down), whatever the distance scrolled.
- **Keyboard**, from either window (the child window never takes keyboard
  focus, so winit reports keys against the main window), but only while
  egui does not want the keyboard: `qemu_takes_keys` is false while an
  egui text field such as the address bar has focus
  (`Context::wants_keyboard_input()`). Each key press or release gives
  `(3, evdev, 1 or 0)` via `keycode_to_evdev()`; keys without a mapping are
  logged ("[qemu-gfx] no evdev mapping for ..., ignoring") and skipped. A
  press also gives one `(5, code point, 0)` per character of
  `logical_key.to_text()`, except for the keys in `handled_as_qcode`
  (Tab, Backspace, Delete, the arrows, Home, End, PageUp, PageDown).

### Event protocol

| Type | Meaning | `code` | `value` | Produced by |
|---|---|---|---|---|
| 1 | Pointer position | 0 = x, 1 = y | Pixel in the window, i.e. in the zoomed frame | `CursorMoved`; headless `!move` |
| 2 | Pointer button | QEMU `InputButton`: 0 left, 1 middle, 2 right, 3 wheel up, 4 wheel down, 5 side, 6 extra | 1 down, 0 up | `MouseInput`, `MouseWheel`; headless `!click`, `!down`, `!up` |
| 3 | Key | Linux evdev code | 1 down, 0 up | `KeyboardInput`; headless typing |
| 4 | Resize | Width | Height | Nobody. QEMU prints it and ignores it |
| 5 | Text | Unicode code point | 0 | Key presses with text; headless typing. Used by text consoles only |

**Note:** the comment on `InputEvent` in `src/qemu_gfx.rs` lists types 1
to 4 only; type 5 came later.

### How QEMU consumes them

`wasi_gfx_display_poll_events()` runs at the start of each refresh:

1. It tops up its own buffer of up to 128 events (`MAX_EVENTS`) from
   `host_gfx_poll_events()`.
2. Type 1: the value is clamped to the zoomed frame and passed to
   `qemu_input_queue_abs()` as an absolute position. Type 2:
   `qemu_input_queue_btn()`, then `qemu_input_event_sync()` after each
   button event (a pointing device builds one report per sync, so a quick
   press and release in one batch would cancel out). Type 3: key handling
   below. Type 5: text handling below. Type 4: printed, ignored.
3. After `KEYS_PER_POLL` (6) key or text events it stops; the remaining
   events, of every type, wait for the next refresh so that their order is
   kept. Why: the guest's input devices buffer little (a PS/2 keyboard
   16 bytes, a USB keyboard 16 events, a QEMU text console 16
   characters), and keys typed during a stall of the main loop (for
   example while the host moves QEMU to a fresh store) used to arrive as
   one burst and get partly lost.

Keys (`wasi_gfx_handle_key()`): Ctrl and Alt are tracked; Ctrl+Alt+digit
switches consoles (below); the evdev code becomes a QEMU key code with
`qemu_input_linux_to_qcode()` ("wasi-gfx: no qcode for evdev N" if there
is none). On the graphic console, presses and releases go to the guest's
keyboard. On a text console only presses count, and only the navigation
keys mean anything (Tab, Backspace, Delete, arrows, Home, End, PageUp,
PageDown).

Text (`wasi_gfx_handle_text()`): ignored on the graphic console. On a
text console Ctrl+letter becomes the control character (Ctrl+A is 1, and
so on, which the monitor's line editing uses), and the character is typed.

So each key press reaches QEMU twice, as a key and as text, and each kind
of console uses one of them. That is why `handled_as_qcode` sends no text
for the navigation keys: a text console would get them twice.

## Keyboard details

Physical keys (`PhysicalKey::Code`) are key *positions*, independent of
the keyboard layout, and evdev codes describe positions too, so the
guest's own layout decides which character a key types. Text events come
from the host's layout (`logical_key`), so on text consoles the host's
layout applies. evdev codes are a stable public numbering, which keeps the
protocol independent of QEMU's internal key-code enum.

`keycode_to_evdev()` covers: Escape, F1-F24; the number row with
Backquote, Minus, Equal, Backspace; letters and punctuation with Tab,
Enter, CapsLock, both Shifts, both Controls, both Alts, Space; Super
(Windows/Command) keys and ContextMenu; PrintScreen, ScrollLock, Pause,
Insert, Delete, Home, End, PageUp, PageDown, the arrows; the numeric
keypad; IntlBackslash, IntlRo, IntlYen, Convert, NonConvert, KanaMode,
Lang1, Lang2; volume mute/down/up and Power. Everything else (Fn, Help,
browser and media-player keys, F25 and up, ...) is ignored.

To add a key:

1. Find the winit `KeyCode` variant, for example `KeyCode::MediaPlayPause`.
2. Look up its evdev code in `linux/input-event-codes.h`: `KEY_PLAYPAUSE`
   is 164.
3. Check that QEMU knows it: in qemu-wasi, the table generated during the
   build, `qemu-build/ui/input-keymap-linux-to-qcode.c.inc`, maps 164 to
   `Q_KEY_CODE_AUDIOPLAY`. `Q_KEY_CODE_UNMAPPED` means QEMU drops it.
4. Add `KeyCode::MediaPlayPause => 164,` to `keycode_to_evdev()`.
5. If QEMU's text consoles treat it as a navigation key (it appears in
   `qcode_to_keysym` in QEMU's `ui/console.c`), add it to
   `handled_as_qcode` in `App::window_event()`.
6. Rebuild and press the key with `WASI_DEBUG=1` set: QEMU logs
   `wasi-gfx: key evdev=164 down=1 qcode=...` for each key event.

Whether the guest reacts also depends on the emulated keyboard being able
to send that key. The headless typist has its own character table,
`char_to_evdev()` in `src/headless.rs`.

## Consoles: Ctrl+Alt+1/2/3

QEMU numbers its consoles from 0, graphic consoles first, then text
consoles in creation order. With the GUI's `-monitor vc -serial vc` that
gives:

| Keys | Console | Shows |
|---|---|---|
| Ctrl+Alt+1 | 0 | The guest's screen |
| Ctrl+Alt+2 | 1 | QEMU's monitor |
| Ctrl+Alt+3 | 2 | The guest's serial port |

Ctrl+Alt+n (n from 1 to 9) shows console n-1 and sends input to it
(`wasi_gfx_display_switch_console()`). The hotkey is consumed: the guest
never sees Ctrl+Alt+1..9, the text event of the same key press is
swallowed, and leaving the graphic console releases Ctrl and Alt in the
guest so that they do not stay pressed. A console that does not exist
gives "wasi-gfx: no console N". `WASI_GFX_CONSOLE=<n>` chooses the console
shown at startup (default: the first graphic one). Text consoles are
640x384 pixels (80x24 characters), so switching also resizes the frame.

## Scaling

The backend zooms the frame by a whole number (nearest neighbour, so text
stays sharp): `WASI_GFX_SCALE`, default 2, limited to 1-8, read by QEMU
(m-browser passes `WASI_*` variables through). For each screen size the
factor is lowered until the result fits in 1600x1200, but never below 1:

| Guest screen | Shown as |
|---|---|
| VGA text mode, 720x400 | 1440x800 (x2) |
| Text console, 640x384 | 1280x768 (x2) |
| 1024x768 | 1024x768 (x1) |
| 1280x800 | 1280x800 (x1) |

## Pitfalls

**Pitfall:** coordinates are window pixels of the zoomed frame, which is
drawn 1:1 at the child window's top-left corner; QEMU clamps positions to
the frame. The child window is 1080x720 logical pixels and is not resized
to the frame: on a display with scale factor 1 that is smaller than a
1280x800 frame, which is then cut off at the right and bottom.

**Pitfall:** absolute positions need an absolute pointing device, such as
`-device usb-tablet` (on `-device qemu-xhci`). Without one, QEMU's input
layer finds no device that takes absolute events and drops them; buttons
still reach a PS/2 mouse, wherever its pointer is.

**Pitfall:** the refresh only runs while QEMU's main loop runs. Frames and
input polling stop while QEMU starts up, while the host moves it to a
fresh store, and (single-threaded build) during each vCPU time slice of
about 30 ms (`QEMU_TB_SLICE_MS`). Input then waits in the queue and
arrives later, paced as above.

**Pitfall:** keys go to QEMU wherever the pointer is, as long as no egui
text field has focus. If typing stops reaching the guest, click somewhere
outside the address bar. See also the click-through pitfall in
[05-browser-ui.md](05-browser-ui.md#the-child-window).

**Note:** winit's auto-repeat presses are forwarded as more key-down
events, without releases in between. The host's desktop may take some key
combinations (Alt+Tab, Super, Ctrl+Alt+F1, ...) before m-browser sees
them.

## Debugging

| Setting or log | What it tells you |
|---|---|
| `[qemu-gfx] host_gfx_init(w, h)`, `host_gfx_resize(w, h)` | The frame size m-browser draws |
| `wasi-gfx: initializing display ...`, `wasi-gfx: resize to ...` | The size and zoom on QEMU's side |
| `wasi-gfx: switching to text console 2` | A console switch (always printed) |
| `WASI_DEBUG=1` | QEMU-side tracing, including every key event with its evdev code and QEMU key code (also much other startup tracing) |
| `QEMU_DISPLAY_STATS=1` | Every ~20 s, what display refreshes cost |
| `WASI_GFX_FULL_REFRESH=1` | Full redraw on every refresh: rules out missed screen updates |
| `QEMU_SNAPSHOT=/tmp/screen.png` (headless) | The current frame as a file |
