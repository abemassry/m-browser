# Headless runner and automation

**What this covers.** m-browser's `--qemu-headless` mode (`src/headless.rs`):
running a QEMU wasm module from a terminal or a script, without a window,
with screenshots written to a PNG file, text and mouse clicks fed in from
a schedule or a named pipe, and a clean shutdown after a set time. Read it
to test QEMU changes, to script the guest, or to understand the qemu-wasi
test and measurement scripts that are built on it.

## Summary

```text
m-browser --qemu-headless [--preopen HOST=GUEST]... <qemu.wasm> [QEMU args...]

QEMU_SNAPSHOT=<file.png>   the screen as a PNG, at most once a second
QEMU_TYPE=<script>         text typed on a timer
QEMU_TYPE_FIFO=<path>      text and pointer commands from a named pipe
QEMU_QUIT_AFTER=<seconds>  quit cleanly after this long
```

The runner calls the same `wasm::run_wasm_module()` as the GUI, with no
window: frames go to `QEMU_SNAPSHOT` instead of the screen, and input
comes from typist threads instead of winit. Everything QEMU-related in
[06-qemu-runner.md](06-qemu-runner.md) and
[09-display-and-input.md](09-display-and-input.md) applies unchanged.

## Usage and arguments

```sh
# from the m-browser directory
./target/release/m-browser --qemu-headless [--preopen HOST=GUEST]... \
    <qemu.wasm> [QEMU arguments...]
```

1. `--qemu-headless` must be the first argument; `main()` passes the rest
   to `headless::run()`.
2. Leading `--preopen HOST=GUEST` pairs add preopened directories (split
   at the first `=`): QEMU sees host directory HOST as GUEST. They must
   come before the module path, as parsing stops at the first other
   argument.
3. The next argument is the module: any path, with or without `.wasm`
   (for example qemu-wasi's `qemu-build/qemu-system-x86_64`).
4. The rest goes to QEMU verbatim, after `argv[0]`
   (`qemu-system-x86_64`). Unlike the GUI, the runner adds no QEMU
   options, except `-smp` for the multi-threaded build when it is
   missing (`run_wasm_module()` does that).

As in the GUI, `pc-bios` is preopened as `/bios` and `.` as `/data`, both
relative to the working directory, and `QEMU_*`/`WASI_*` environment
variables reach QEMU.

**Pitfall:** run it from the m-browser directory (or any directory with a
`pc-bios` subdirectory): otherwise preopening `pc-bios` fails and the
runner stops with an error.

**Pitfall:** the runner takes no disk image lock (that is
`lock_disk_image()` in the GUI). Two QEMUs on one qcow2 image (QEMU's
disk image format) corrupt it. Run headless QEMUs on a copy of the image,
never on the one a GUI may be using.

## Environment variables

| Variable | Effect |
|---|---|
| `QEMU_SNAPSHOT=<file.png>` | Write the screen to this PNG at most once a second, through a temporary file renamed into place. Its size is the zoomed frame (see `WASI_GFX_SCALE`) |
| `QEMU_TYPE=<script>` | Type text on a timer ([below](#qemu_type)) |
| `QEMU_TYPE_FIFO=<path>` | Type every line written to this named pipe, followed by Enter; lines starting with `!` are pointer commands ([below](#qemu_type_fifo)) |
| `QEMU_QUIT_AFTER=<seconds>` | After this long (decimals allowed), ask QEMU to quit, as the GUI does when its window closes: power off, close the disk images, at most 10 s |

Times count from the start of m-browser, which includes loading (and on
the first run, compiling) QEMU's module, not from the guest's boot. Also
useful headless: `WASI_GFX_CONSOLE=<n>` (console shown, and typed into,
from the start), `WASI_GFX_SCALE`, and `QEMU_PROFILE`
([12-profiling.md](12-profiling.md)). All variables are listed in
[reference/environment-variables.md](reference/environment-variables.md).

### QEMU_TYPE

Entries `<seconds>:<text>` separated by `;`, sorted by time. The text may
contain `:` (the entry is split at the first one) but not `;`. `\n` types
Enter, `\t` Tab and `\e` Escape; these are a backslash and a letter in
the variable, so single-quote the value in the shell:

```sh
QEMU_TYPE='300:root\n;310:uname -a\n'
```

### QEMU_TYPE_FIFO

A named pipe (*FIFO*) is a special file made with `mkfifo`: what one
process writes into it, another reads. The runner opens it (which waits
until a writer appears), handles each line, and reopens it when the
writer closes it, so each `echo ... > fifo` works.

| Line | Effect |
|---|---|
| any text | Typed, then Enter. Escapes such as `\n` are not interpreted |
| `!move X Y` | Pointer to (X, Y) |
| `!click B` | Press and release button B |
| `!down B`, `!up B` | Press, release button B |

X and Y are pixels of the zoomed frame, the same pixels as the
`QEMU_SNAPSHOT` image, so you can measure positions on a snapshot. B is a
QEMU button code: 0 left, 1 middle, 2 right, 3 wheel up, 4 wheel down.
Unknown commands are logged and ignored. Pointer positions need an
absolute pointing device in the guest (`-device qemu-xhci -device
usb-tablet`).

**Pitfall:** the path must be a FIFO. A regular file is read to its end,
reopened at once and typed again, forever. A path that cannot be opened
gives "[headless] cannot open ..." and no typing at all.

**Pitfall:** `echo ... > fifo` blocks until the runner has the pipe open,
and hangs if the runner has exited; use `timeout 5 sh -c '...'` in
scripts.

## How text becomes key presses

For each character `type_text()` queues:

1. If `char_to_evdev()` knows the character: Left Shift down if needed,
   the key down, the key up, Shift up. This is for the guest's keyboard
   (the graphic console).
2. Always: a text event carrying the character, for QEMU's text consoles
   (monitor, serial port).

Then it waits 60 ms, roughly human typing speed. Each kind of console
ignores the other kind of event (see
[09-display-and-input.md](09-display-and-input.md)).

`char_to_evdev()` assumes a US keyboard layout in the guest. It knows
letters (capitals with Shift), digits, space, US punctuation
(`-_=+[{]};:'"\|,<.>/?`, `!@#$%^&*()`, the backquote and `~`), Enter
(`\r` and `\n`), Tab, Escape and Backspace (`\x08` and `\x7f`). Any other
character (`é`, `€`, other control characters) is sent as text only: it
reaches text consoles, not the guest's keyboard.

Enter: `\n` in `QEMU_TYPE` and the end of every FIFO line become `\r`,
that is the Enter key for the guest and character 13 for text consoles,
which is what a terminal's Enter key sends.

**Pitfall:** on a text console, Tab and Backspace act twice, as a key
(text consoles handle those keys) and as text. The GUI avoids this
(`handled_as_qcode` in `src/app.rs`); the headless typist does not.

**Keys can be lost.** m-browser's queue never drops anything, and QEMU
delivers at most 6 key or text events per display refresh of about
30 ms (`KEYS_PER_POLL`), which the 60 ms pace stays below (3 events per
character, 5 with Shift). But a guest that is too busy to read its
keyboard, such as one still booting or starting X, can still drop keys,
and text sent before the guest is ready is usually lost. Type when a
prompt is visible (in a snapshot or the serial log) and check the result.

## Output and exit codes

QEMU's stdout and stderr are m-browser's own (WASI inherits them):
QEMU's messages (`wasi-gfx: ...`, `[qemu-wasi] ...`, errors) are mixed
with m-browser's, which start with `[qemu]`, `[qemu-jit]`, `[qemu-gfx]`,
`[qemu-net]`, `[qemu-profile]` or `[headless]`. Redirect both to files in
scripts. The guest's serial port goes wherever `-serial` says:
`-serial file:/out/serial.log` (with a preopened `/out`) records it,
`-serial vc` makes it a text console that typing reaches and snapshots
show.

| Situation | Exit code |
|---|---|
| QEMU ran and ended: the guest powered off, `QEMU_QUIT_AFTER`, `quit` in the monitor, `exit()` on QEMU's main thread | 0, whatever QEMU's status (the log says `[qemu] exited with status N`) |
| Bad runner arguments or variables, module not loadable, QEMU exited during startup (bad option, missing disk image), QEMU trapped | 1, after `[headless] error: ...` |
| Multi-threaded build: another QEMU thread called `exit()` or trapped | QEMU's status, or 1 for a trap (`thread_failed()`) |

Killing the runner (Ctrl+C) stops QEMU without closing the disk image:
acceptable for a throw-away copy only. For a clean end use
`QEMU_QUIT_AFTER`, power the guest off, or type `quit` into the monitor.

## Worked examples

All commands run from the m-browser directory, on a copy of the disk
image (a new copy for each run you kill):

```sh
mkdir -p ~/vm-test && cp m.qcow2 ~/vm-test/disk.qcow2
```

**1. Boot headless and save screenshots.** The GUI's arguments, with the
copy as the disk; QEMU quits cleanly after 15 minutes:

```sh
QEMU_SNAPSHOT=$HOME/vm-test/screen.png QEMU_QUIT_AFTER=900 \
./target/release/m-browser --qemu-headless --preopen "$HOME/vm-test=/out" \
    qemu-system-x86_64.wasm -L /bios \
    -drive file=/out/disk.qcow2,format=qcow2 -m 1024 \
    -accel tcg,tb-size=256 -device qemu-xhci -device usb-tablet \
    -monitor vc -serial vc -nic user \
    > ~/vm-test/stdout.log 2> ~/vm-test/stderr.log
```

`screen.png` is overwritten as the screen changes; to keep a series, copy
it in a loop from another terminal:

```sh
while sleep 10; do cp ~/vm-test/screen.png ~/vm-test/shot-$(date +%s).png; done
```

**2. Type into a root shell on the serial console.** This needs a guest
with a login prompt on its first serial port (ttyS0). The serial console
(console 2 with `-monitor vc -serial vc`) is shown and typed into from
the start:

```sh
mkfifo /tmp/type.fifo
WASI_GFX_CONSOLE=2 QEMU_TYPE_FIFO=/tmp/type.fifo \
QEMU_SNAPSHOT=$HOME/vm-test/screen.png \
./target/release/m-browser --qemu-headless --preopen "$HOME/vm-test=/out" \
    qemu-system-x86_64.wasm -L /bios \
    -drive file=/out/disk.qcow2,format=qcow2 -m 1024 \
    -accel tcg,tb-size=256 -monitor vc -serial vc -nic user \
    > ~/vm-test/qemu.log 2>&1 &
# once screen.png shows the login prompt:
echo root > /tmp/type.fifo
echo 'uname -a' > /tmp/type.fifo
```

The answers appear in `screen.png`. Without a serial login prompt, leave
out `WASI_GFX_CONSOLE` and type into the guest's own screen instead. For
a ready-made root shell in the guest, see `guest-shell.sh` in qemu-wasi's
`tools/perf/`.

**3. Click at a position.** With example 1 started with
`QEMU_TYPE_FIFO=/tmp/type.fifo` added, take the position from
`screen.png`:

```sh
echo '!move 640 400' > /tmp/type.fifo
echo '!click 0' > /tmp/type.fifo
```

**4. Wait for a pattern in the serial log.** The guest writes to its
serial port if its kernel console is there (`console=ttyS0`), if a login
prompt runs there, or if a program writes to `/dev/ttyS0`:

```sh
#!/bin/sh
# wait-serial.sh PATTERN [SECONDS]: boot ~/vm-test/disk.qcow2 headless and
# wait until the guest prints PATTERN on its serial port. Run from the
# m-browser directory.
set -eu
pattern=$1
limit=${2:-900}
out=$HOME/vm-test
rm -f "$out/serial.log"
QEMU_QUIT_AFTER=$((limit + 60)) \
./target/release/m-browser --qemu-headless --preopen "$out=/out" \
    qemu-system-x86_64.wasm -L /bios \
    -drive file=/out/disk.qcow2,format=qcow2 -m 1024 \
    -accel tcg,tb-size=256 -serial file:/out/serial.log -nic user \
    > "$out/qemu.log" 2>&1 &
pid=$!
waited=0
until grep -q -- "$pattern" "$out/serial.log" 2>/dev/null; do
    kill -0 "$pid" 2>/dev/null || { echo "QEMU stopped" >&2; exit 1; }
    [ "$waited" -lt "$limit" ] || { echo "timeout" >&2; kill "$pid"; exit 1; }
    sleep 1
    waited=$((waited + 1))
done
echo "found after $waited s"
kill "$pid"   # a throw-away copy: copy the image again before the next run
```

## Built on the headless runner

- qemu-wasi's instruction test, `tests/x86-insn-diff/run.sh` (`make
  test`), runs a small test kernel under the wasm QEMU from the m-browser
  directory with `--preopen "$out=/out" --preopen "$src/pc-bios=/bios2"`,
  collects its output through a file (`-chardev file,...` with
  `-device isa-debugcon`), and compares it with a native QEMU's.
- qemu-wasi's `tools/perf/` scripts (boot timing, click latency, guest
  benchmarks) use snapshots, the FIFO and serial logs; see
  tools/perf/README.md and docs/15-performance.md in the qemu-wasi
  repository.
