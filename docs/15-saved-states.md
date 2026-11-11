# Saved states: pausing, saving and resuming wasm programs

**What this covers.** How m-browser pauses a running wasm program, saves
its complete state to a file, and resumes it later from that file, also
in a new m-browser process: the ideas behind it (save points, Asyncify,
hibernation), the `--wasm-headless` runner that does it for core modules,
how to prepare a program for it, QEMU's checkpoints (made by QEMU's own
migration and resumable several times at once), the file format, how the
code works (`src/saved_state.rs`, `src/saved_state/snapshot.rs`,
`src/saved_state/wasi.rs`, `src/wasm_headless.rs`, `src/qemu_save.rs`),
what can go wrong, and the tests. Read it to use the feature, before
changing that code, or before adding saved states to another kind of
program. Terms such as Store, instance, Linker and WASI are explained in
[02-concepts.md](02-concepts.md).

## Status

| Program | Pause | Save and resume |
|---|---|---|
| Core module run by `--wasm-headless --step` (a reactor) | yes, between steps | yes |
| Core module run by `--wasm-headless`, transformed with Asyncify | yes, at its WASI calls | yes |
| Other core modules run by `--wasm-headless` | yes, at their WASI calls | no: no save points |
| Core modules with threads | cannot run: the runner has no `wasi.thread-spawn` | no |
| QEMU, multi-threaded build, `--qemu-headless` | yes | checkpoints, resumable several times at once ([QEMU](#qemu-checkpoints)) |
| QEMU, single-threaded build, `--qemu-headless` | yes | no: QEMU writes its state from a thread |
| wasi-gfx components (`triangle.wasm`, `breakout.wasm`) | in the GUI, while their tab is in the background (wasmtime epochs) | no ([Limits](#limits)) |
| The GUI's tabs | yes: a tab's program (QEMU, a component, another core module) pauses while the tab is in the background ([05-browser-ui.md](05-browser-ui.md#pausing-kind-by-kind)) | the session: core modules with save points hibernate when the browser quits and resume when their tab is next opened; other tabs come back by address ([05-browser-ui.md](05-browser-ui.md#the-session)) |
| Web tabs (Chromium in QEMU) | yes | each starts from the one web checkpoint, a QEMU checkpoint saved from the GUI ([05-browser-ui.md](05-browser-ui.md#web-tabs)) |

In the GUI, each tab has an id of 12 hex digits (`Tab::id`), and the
saved states and disk overlays of its programs carry it as their owner
(`Saving::owner` for QEMU, `Options::owner` for other core modules), as
the headless runners carry the id of their session. A `.wasmsavestate`
file opened as a page resumes in the tab.

## The ideas

A running wasm program has **wasm state**, which lives in its instance:
linear memories, globals and tables. It also has **host state**, which
lives in m-browser: open files, clocks, windows, sockets. Saving means
writing both into a file; resuming means making a new instance and new
host objects, in a new process if need be, and putting the state back.

The hard part is the **call stack**. While a wasm function runs, its
local variables and return addresses are on the native stack of the
m-browser process, in a form only wasmtime understands. wasmtime 41 can
read those frames (with `Config::guest_debug`) but cannot rebuild them.
So a program can only be saved at a **save point**: a moment when none
of its functions is running.

```text
 a reactor (--step)                a command transformed by Asyncify

 host: step()   step()   step()    host: _start() ...............................
             ^        ^                  main > loop > work > fd_write (wrapped)
     save points: between two            1. the host asks it to unwind: each
     calls, no frame is live                function copies its locals into a
                                            buffer in memory, and returns
                                         2. _start returns: a save point
                                         3. on resume, _start again: each
                                            function restores its locals from
                                            the buffer, down into fd_write
```

- A **reactor** returns to the host after each call, so the gap between
  two calls is a save point. QEMU is a reactor: m-browser calls its
  `wasi_qemu_step` in a loop. `--step EXPORT` runs any module this way.
- A **command** (`_start`, a C or Rust `main`) runs until it ends. It
  has a save point only if it has been transformed with Binaryen's
  **Asyncify** (`wasm-opt --asyncify`), which teaches every function to
  copy its locals into linear memory and return (*unwind*), and later to
  restore them and go back into the call it left (*rewind*). After an
  unwind no frame is live, and the stack is just bytes in memory, saved
  with the rest.

**Hibernate** means that saving ends the run, and the program continues
from the file. **Checkpoint** means that the program keeps running after
the save. `--wasm-headless` hibernates, as decided for programs other
than QEMU.

A saved program must find the same world when it resumes: the same
module (checked by SHA-256), the same preopened directories, and the
same open files, unchanged (checked by size and modification time).
Its monotonic clock continues from the value it had when it was saved,
so the program never sees it jump or go back.

## Using it: the headless runner

```sh
# any directory; saved states are written to the current one
m-browser --wasm-headless [--preopen HOST=GUEST]... [--env KEY=VALUE]... \
    [--step EXPORT] <module.wasm> [args...]
m-browser --wasm-headless --resume <file.wasmsavestate>
```

| Argument | Meaning |
|---|---|
| `--preopen HOST=GUEST` | Make host directory HOST visible to the program as GUEST (WASI preopen). Repeatable |
| `--env KEY=VALUE` | An environment variable for the program. Repeatable; nothing else of m-browser's environment reaches it |
| `--step EXPORT` | The module is a reactor: call `_initialize` if it exports one, then EXPORT (no parameters, an `i32` result) in a loop until it returns 0 or more, its exit status |
| `<module.wasm> [args...]` | The core module, and its arguments; `argv[0]` is the module's file name |
| `--resume FILE` | Continue the program saved in FILE. The module, arguments, environment and preopens come from the file |

| Variable | Effect |
|---|---|
| `WASM_CONTROL_FIFO=<path>` | Read commands from this named pipe (`mkfifo`), one per line: `pause`, `continue`, `hibernate`, `quit` |
| `WASM_HIBERNATE_AFTER=<s>` | Hibernate at the first save point after s seconds (fractions allowed) |

The program uses m-browser's standard input and output. m-browser's own
messages go to standard error and start with `[wasm]`. The exit status
is the program's, or 0 after it hibernated or was stopped by `quit`; 1
with `[wasm] error: ...` when m-browser could not run it.

Saved states are named `<module>-<UTC time>-<counter>-<session>.wasmsavestate`,
for example `counter-20261029T181442Z-1-6a6fae873111.wasmsavestate`.
The session id (12 hex digits) is chosen when a program starts and kept
by every resume, and the counter counts the session's saves, so the
files of one run sort together and never collide.

**Example: a C command.** It appends a numbered line with the elapsed
time to a file every 100 ms:

```c
/* counter.c */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 30;
    struct timespec t0, t;
    FILE *f = fopen("/data/counter.log", "w");
    if (!f) { perror("fopen"); return 1; }
    clock_gettime(CLOCK_MONOTONIC, &t0);
    for (int i = 1; i <= n; i++) {
        clock_gettime(CLOCK_MONOTONIC, &t);
        fprintf(f, "%d %ld\n", i, (long)((t.tv_sec - t0.tv_sec) * 1000 + (t.tv_nsec - t0.tv_nsec) / 1000000));
        fflush(f);
        printf("tick %d\n", i);
        usleep(100 * 1000);
    }
    return fclose(f);
}
```

Built with wasi-sdk, transformed with Asyncify, hibernated after 1.5 s,
and resumed in a new process; `data/counter.log` then holds lines 1 to
30 in order:

```sh
# in a scratch directory with a data/ subdirectory
/opt/wasi-sdk/bin/clang --target=wasm32-wasip1 -O2 counter.c -o counter.wasm
wasm-opt --asyncify -O2 counter.wasm -o counter.async.wasm
WASM_HIBERNATE_AFTER=1.5 m-browser --wasm-headless --preopen data=/data counter.async.wasm 30
# [wasm] hibernated to .../counter.async-20261029T181442Z-1-6a6fae873111.wasmsavestate
m-browser --wasm-headless --resume counter.async-*-1-*.wasmsavestate
```

**Example: a reactor, controlled by hand.**

```sh
mkfifo ctl.fifo
WASM_CONTROL_FIFO=ctl.fifo m-browser --wasm-headless --preopen data=/data \
    --step step reactor.wasm &
echo pause > ctl.fifo        # the program stops between two steps
echo continue > ctl.fifo
echo hibernate > ctl.fifo    # saved, m-browser exits
m-browser --wasm-headless --resume reactor-*.wasmsavestate
```

A C reactor is built with `clang -mexec-model=reactor` and exports its
step function with `__attribute__((export_name("step")))`.

**Note:** `WASM_HIBERNATE_AFTER` and `WASM_CONTROL_FIFO` are read again
by `--resume`: a resumed run with `WASM_HIBERNATE_AFTER` still set
hibernates again, which is how a program can be carried through several
processes.

**Pitfall:** `WASM_CONTROL_FIFO` must be a named pipe. A regular file is
read to its end and opened again at once, forever, as with the QEMU
runner's `QEMU_TYPE_FIFO`.

## Preparing a command with Asyncify

```sh
wasm-opt --asyncify -O2 program.wasm -o program.async.wasm
```

`wasm-opt` is part of Binaryen. m-browser recognizes a transformed module
by the five exports Asyncify adds (`asyncify_start_unwind`,
`asyncify_stop_unwind`, `asyncify_start_rewind`, `asyncify_stop_rewind`,
`asyncify_get_state`).

| | |
|---|---|
| Save points | Calls to the WASI functions m-browser wraps: `fd_read`, `fd_write`, `fd_pread`, `fd_pwrite`, `poll_oneoff` (sleeping), `clock_time_get`, `random_get`, `sched_yield`, `path_open`, `fd_close`, `fd_renumber`. A program in a loop that calls none of them can be neither paused nor saved until it does |
| Memory | The unwound stack goes into 1 MiB that m-browser adds at the end of memory 0 the first time (`asyncify_buffer`); the program's allocator never uses pages it did not grow itself |
| Size | Measured: breakout's main module 23.9 MB after `wasm-opt -O1`, 38.5 MB after `wasm-opt -O1 --asyncify` (+61%); triangle's 82 KB and 144 KB (+75%); the C example 211 KB from clang and 224 KB after `wasm-opt --asyncify -O2` |
| Speed | Not measured. Every function that can reach a save point checks Asyncify's state after each call |
| Cannot | Modules using wasm exceptions: Binaryen aborts (`Flatten.cpp: unexpected expr type`). QEMU is one, but it is a reactor and needs no Asyncify |

**Pitfall:** Binaryen needs about 2 GB of memory and half a minute for a
40 MB module.

## QEMU checkpoints

QEMU is saved another way. The multi-threaded QEMU's threads are always
inside wasm functions, so it has no save point, and its state is mostly
the guest's RAM. But QEMU can save itself: its **migration**, made to
move a running virtual machine to another host, writes the guest's RAM
and device state to a file (`migrate file:`), and a new QEMU started with
the same arguments loads it (`migrate_incoming`). m-browser drives this
with QEMU's **monitor** commands (what you type at its `(qemu)` prompt),
which reach QEMU through qemu-wasi's `wasi_qemu_monitor` export.

QEMU makes **checkpoints**: the guest runs on after a save. A checkpoint
can be resumed any number of times, also several at once, each copy on a
disk of its own; that is what new browser tabs are to start from.

### Using it

```sh
# from the m-browser directory, whose ./saves QEMU sees as /data/saves
mkdir -p saves; mkfifo /tmp/type.fifo
QEMU_SAVE_DIR=saves QEMU_TYPE_FIFO=/tmp/type.fifo \
    m-browser --qemu-headless qemu-system-x86_64.wasm -L /bios -m 1024 \
    -drive file=/data/disk.qcow2,format=qcow2 ... &
echo '!save' > /tmp/type.fifo
# [qemu] saved a checkpoint to .../saves/qemu-system-x86_64-20261029T213832Z-1-58df7b2ad2ec.wasmsavestate
m-browser --qemu-headless --resume saves/qemu-system-x86_64-*-1-58df7b2ad2ec.wasmsavestate
```

| FIFO line | Effect |
|---|---|
| `!pause` | Stop the guest (`stop`); its clocks stand still |
| `!continue` | Let it run again (`cont`) |
| `!save` | Write a checkpoint; the guest goes on |
| `!monitor CMD` | Run a monitor command, such as `info status`, and print its output |

`--resume FILE` takes the module, QEMU's arguments and the preopened
directories from the checkpoint; more `--preopen`s may be added. Each
copy gets a session id of its own, and its saves are counted from 1.
`QEMU_SAVE_DIR` (default: the working directory) is where checkpoints
and disk overlays go; QEMU must reach it through a preopen.

### The disks

The guest's disks are not in QEMU's migration stream, and the guest
writes them all the time. So a save **freezes** them: QEMU gets a new
qcow2 overlay for each writable disk (`snapshot_blkdev`), writes there
from then on, and the image it wrote before becomes read-only, part of
the checkpoint. A resumed copy starts on the frozen images and gets
overlays of its own the same way, before it loads the state:

```text
before the save        after the save            after two resumes
disk.qcow2 (rw)        disk.qcow2 (frozen)       disk.qcow2 (frozen)
                         ^                         ^       ^       ^
                       A's overlay (rw)          A's     B's     C's
                       saves/...-1-<A>.disk0     overlay overlay overlay
```

**Pitfall:** after the first save, QEMU never writes the image named in
`-drive` again: it belongs to the checkpoint, and what the guest writes
later is in the overlays in `QEMU_SAVE_DIR`. Starting QEMU on that image
again (a fresh boot) writes it, and the checkpoint then refuses to resume
(`... changed since the checkpoint was saved`). Overlays are never
removed by m-browser; an overlay is needed by the checkpoints saved
after it, and frozen images by every checkpoint above them.

### What a save and a resume do

```text
save (between two steps)                 resume (--resume FILE)
 stop (if it runs)                        check the file, its module, its
 snapshot_blkdev <each disk> <overlay>      frozen images (prepare_resume)
 migrate -d file:<FILE>.tmp,offset=64K    start QEMU with the saved arguments
 ... QEMU's migration thread writes;        and -incoming defer
     each step: info migrate              snapshot_blkdev <each disk> <overlay>
 "completed": manifest into the first     migrate_incoming file:<FILE>,offset=64K
     64 KiB, rename to FILE               ... each step: info migrate
 cont                                     "completed": cont, if it ran when saved
```

The guest is stopped during a save, so that its memory and its frozen
disks are of the same moment. Measured on the multi-threaded build with
a 1 GiB guest at a shell: a 129 MB checkpoint written in 2 s, and two
copies resumed at once, each running 2 to 4 s after starting.

The file is a saved state ([below](#the-file)) whose manifest, header
and `QEMU` section, is in the first 64 KiB (`STATE_OFFSET`), followed by
QEMU's stream as QEMU wrote it. The `QEMU` section holds where the
stream is, its length and CRC32, whether the guest was running, and each
disk: the device, the frozen image, and the identity (device, inode,
size, modification time) of every file in its qcow2 backing chain.
Before a resume, `prepare_resume()` checks the module's SHA-256, those
files, and the stream's length and CRC32.

### How it works: `src/qemu_save.rs`

| Part | What it does |
|---|---|
| `monitor()` | Writes the command into the buffer `wasi_qemu_buffer` returns, calls `wasi_qemu_monitor`, reads the output back from the buffer |
| `QemuControl`, `Request`, `Report` | Requests from any thread: the FIFO reader, or the GUI, which sends `Pause` and `Continue` as QEMU's tab goes to the background and comes back, and `Save` for the web checkpoint. Reports back (`QemuControl::reports()`): `Resumed` when a resume has loaded, `Saved(path)`, `Failed(message)`; the GUI reads them on every frame |
| `checkpoint_images()` | The host paths of a checkpoint's disk images, from its manifest; the GUI deletes a replaced web checkpoint's copy of `m.qcow2` with it |
| `Saver` | Built by `run_wasm_module` when it is given a `Saving`; `run_qemu_reactor` calls `after_init()` after `wasi_qemu_init` (starts a resume) and `between_steps()` after every step |
| `Session` | QEMU's arguments as they are now (each save changes the `-drive file=`s), its preopens with absolute host paths, the save directory, the owner id and counter |
| `parse_info_block()`, `with_drive_file()`, `image_chain()` | Read `info block`; rewrite `-drive` options; follow a qcow2 backing chain through the preopens |

`monitor()` must only run between two steps: the main thread then holds
QEMU's big lock, as when QEMU's main loop runs a typed command. The
command runs on a temporary monitor that QEMU destroys after it, which is
why the migration is started detached (`-d`) and followed with
`info migrate` at each step.

### Changes to QEMU

In qemu-wasi, each guarded by `WASI_PLATFORM`:

| Change | Why |
|---|---|
| `wasi_qemu_buffer`, `wasi_qemu_buffer_size`, `wasi_qemu_monitor` exports (`system/main.c`) | m-browser's way into the monitor |
| An incoming `file:` migration starts from a bottom half (`migration/file.c`) | It waited for a "readable" event that the wasm build's `g_poll()` never delivers |
| No periodic yield while loading RAM (`migration/ram.c`) | Coroutines run straight through on WASI: the next schedule found the coroutine scheduled already, and QEMU aborted |
| New image files are created without locks (`block/file-posix.c`) | WASI has no file locks: `snapshot_blkdev` failed with `Failed to lock byte 101` |
| A bottom half that a coroutine runs inline runs outside it (`util/async.c`, `util/coroutine-wasi.c`) | `bdrv_open()` asserted `!qemu_in_coroutine()` when `snapshot_blkdev` opened the overlay |
| `current_cpu` is cleared after the single-threaded vCPU's slice (`accel/tcg/tcg-accel-ops-rr.c`) | `stop` never stopped the single-threaded QEMU |

## The file

```text
+------------------------------------------------------------------+
| "WASMSAVE" | format version u32 | header length u32               |
| header (postcard) | header CRC32 u32                              |
+------------------------------------------------------------------+
| section: tag [u8; 4] | payload length u64 | payload | CRC32 u32   |
|   WASI  open files, closed descriptors, the monotonic clock        |
|   ASYN  where the unwound stack is, and the call it was saved in   |
|   GLOB  values of the mutable globals                              |
|   TABL  sizes of the tables                                        |
|   MEMO  one per memory: size, then each 64 KiB chunk that is not   |
|         all zeros, deflate-compressed on its own                   |
|   END   empty                                                      |
+------------------------------------------------------------------+
```

All numbers are little-endian. The header and the small sections are
serde structs encoded with postcard; `saved_state::Header` holds:

| Field | Meaning |
|---|---|
| `engine` | Which code saved it, so which can resume it: `WasmSnapshot` (`--wasm-headless`) or `QemuMigration` (QEMU) |
| `mode` | `Hibernate` or `Checkpoint` |
| `module` | Absolute path, SHA-256 and size of the module |
| `launch` | How it was started: arguments, environment, preopens (absolute host paths), and whether it is a command or a reactor with its step export; for QEMU, its arguments and preopens |
| `m_browser_version`, `created_unix_secs` | Who saved it, and when |
| `owner`, `counter` | The tab or session, and its save count: the parts of the file name |

`StateWriter` writes to `<name>.tmp<pid>` and renames the file only once
it is complete and synced, so a crash never leaves half a file under the
final name, and a writer dropped unfinished removes its temporary file.
`StateReader` checks the magic bytes, refuses a newer format version,
and checks every section's CRC32. A file in the current format (version
1) contains everything needed to resume it except the module itself and
the files the program had open, which must still be where they were.

## How it works

### The generic engine: `src/saved_state/snapshot.rs`

wasmtime's API reaches only what a module exports, and compilers export
little: C's stack pointer, `__stack_pointer`, is a global no module
exports. So `prepare()` rewrites the module before it is compiled and
adds an export for every memory, table and mutable global it defines,
named `__msave_memory_N`, `__msave_table_N`, `__msave_global_N`, as
Wizer does. Every other section, debug information included, is copied
byte for byte. On QEMU this takes 46 ms and adds 53 bytes.

`prepare()` also reads the module (with wasmparser) and records in its
`Layout` why it could never be saved: a shared memory (threads), or code
that changes tables (`table.set`, `table.grow`, ...). Table entries are
references to functions, which have no form that could go into a file.

| State | `save()` writes | `Restore` does, in a new instance of the prepared module |
|---|---|---|
| Mutable globals | their values | sets them |
| Tables | their sizes, which must be the declared ones | checks them |
| Memories | size, and each 64 KiB chunk that is not all zeros | grows the memory, zeroes what instantiation put there (data segments), copies the chunks in |
| Immutable globals, table contents | nothing | instantiation makes them as they were |

**Note:** the zeroing matters. A data segment that the program had
overwritten with zeros is a zero chunk, absent from the file; without
the zeroing, the new instance would keep the segment's original bytes.

### Files and the clock: `src/saved_state/wasi.rs`

wasmtime-wasi keeps a program's open files in its `WasiP1Ctx`, which
holds host file handles and cannot be saved. `add_to_linker()` wraps
`path_open`, `fd_close` and `fd_renumber` so that a `FileJournal` records
what the program opens: for each descriptor, the preopened directory and
the path in it, and the flags and rights it was opened with. Each
wrapper runs wasmtime-wasi's own code, as wasmtime-wasi's Linker glue
does (`call_p1()`).

To resume, `restore()` opens the same files in a new context with the
same preopens, without `O_CREAT`, `O_EXCL` or `O_TRUNC`, under the same
numbers and at the same offsets. wasmtime-wasi gives a new descriptor the
highest free number, else the next unused one, so files are opened in
ascending order while nothing is free, and placeholders fill the gaps:

```text
saved program:   4 a.txt   5 sub/   6 (free)   7 sub/x.txt       highest used: 7
resume:          open a.txt -> 4; open sub -> 5;
                 open sub/x.txt -> 6 (a placeholder), again -> 7;
                 close the placeholder: 6 is free again
next open:       6 in both
```

Before reopening, `restore()` compares each file with what was recorded
when it was saved: device and inode (the same file), and for files size
and modification time. A changed file refuses the resume; for a disk
image, going on would corrupt it.

`ProgramClock` is the program's monotonic clock (`CLOCK_MONOTONIC`),
given to the context with `WasiCtxBuilder::monotonic_clock()`. Its value
is saved, and a resumed program continues from it; while the program is
paused the clock stands still. wasmtime-wasi's own clock would restart
near 0 in every process.

The other wrapped functions (reading, writing, sleeping, clocks, random
numbers) only call the runner's hook first (`SavePointHook`), which may
pause the program there, unwind it, or stop it.

**Pitfall:** the wrappers use `wasmtime_wasi::p1::wasi_snapshot_preview1`,
code generated inside wasmtime-wasi, which says it is not meant for use
outside. It is public, and Cargo.toml pins wasmtime-wasi and wiggle to
the same version; an upgrade may change it, which shows when compiling.

**Pitfall:** to call a preview-1 function from the host, outside any
call from wasm (as `restore()` does), set the context's hostcall fuel
first (`set_hostcall_fuel`) and run async functions with
`wasmtime_wasi::runtime::in_tokio`. Without fuel, reading a path fails
with errno 48 (`ENOMEM`), which says nothing about the cause.

### The runner: `src/wasm_headless.rs`

`Program::new()` prepares and compiles the module, builds a Linker
(wasmtime-wasi, then the wrappers) and a WASI context (arguments,
environment, preopens, `ProgramClock`), and instantiates it. The Store's
data, `RunnerState`, holds the context, the journal, the clock, the
`Control` and the Asyncify bookkeeping.

`Control` holds the requests (pause, hibernate, quit), set by the FIFO
thread, the timer, or a test. The program asks it what to do at each
`Point`:

| Point | Where | Pause | Hibernate |
|---|---|---|---|
| `Save` | between two steps of a reactor; a wrapped WASI call of a command with Asyncify | waits, clock stopped | yes |
| `Pause` | a wrapped WASI call of another command | waits, clock stopped | declined, with a message |
| `Pass` | a wrapped WASI call inside a reactor's step | no (it pauses between steps) | no |

Hibernating a command, in `save_point()` and `Program::run_command()`:

1. In the wrapped call, `unwind()` writes Asyncify's buffer header,
   calls `asyncify_start_unwind`, and returns without running the call.
2. Every function on the stack saves its locals into the buffer and
   returns; `_start` returns to the runner.
3. The runner calls `asyncify_stop_unwind` and writes the file
   (`Program::hibernate()`): the WASI section, the ASYN section (the
   buffer, and the call it was in), then the wasm state.

Resuming (`resume()`): check the module's SHA-256, `Program::new()`,
restore the wasm state section by section, set the clock, reopen the
files, then run:

- a reactor: the step loop continues, without `_initialize`;
- a command: `asyncify_start_rewind`, then `_start`. Every function
  restores its locals and calls on, down to the wrapped call it was saved
  in. There `save_point()` sees `rewind_to`, checks it is the same call,
  calls `asyncify_stop_rewind`, and lets the call run for real.

If writing the file fails (an unwritable directory, a full disk), the
program goes on: a reactor steps on, and a command, whose stack is still
in its memory, is rewound in place.

## Failure modes

| Message | Meaning |
|---|---|
| `note: this command can be paused but not saved: it has no save points ...` | A command without Asyncify; `hibernate` requests are declined with the same text |
| `note: this program cannot be saved: it shares its memory with threads ...`, `... it changes its function tables ...` | Found by `prepare()`; the program runs, but is never saved |
| `cannot hibernate: its open files cannot be saved: ...` | The program closed a preopened directory or renumbered standard I/O; the request is declined |
| `cannot hibernate, going on: ...` | Writing the file failed; the program continues |
| `<file> changed since the state was saved (its size changed / it was modified / it was replaced by another file); resuming could corrupt it` | A file the program had open is not as it was |
| `<module> has changed since the state was saved (SHA-256 ... then, ... now)` | Another module at that path |
| `<file> is damaged (section MEMO fails its check)`, `... is truncated`, `... is not a saved state` | The file is damaged or not a saved state |
| `<file> was saved by a newer m-browser (file format N ...)` | Update m-browser |
| `cannot resume: the program was saved in X, but its stack came back in Y` | The rewind went elsewhere than the unwind: a bug, or a module changed in a way the hash did not catch |
| `this is a component, not a core module` | `--wasm-headless` runs core modules only |
| `... holds QEMU: resume it with m-browser --qemu-headless --resume`, `... is not a QEMU checkpoint: resume it with m-browser --wasm-headless --resume` | The other runner's file |

QEMU's messages start with `[qemu]`; a failed request is reported as
`[qemu] Save failed: ...` (or `Pause failed`, ...), and QEMU goes on.

| Message | Meaning |
|---|---|
| `this QEMU build cannot take monitor commands from m-browser (it has no wasi_qemu_monitor export)` | A qemu-wasi build older than the monitor export |
| `only the multi-threaded QEMU can save ...` | The single-threaded build's threads never run, QEMU's migration thread included |
| `... is not a directory QEMU can reach (not preopened)` | `QEMU_SAVE_DIR` is outside every preopened directory |
| `disk ... is not one of QEMU's -drive file= arguments` | A disk added another way (`-hda`, `drive_add`); the copy could not be started on its frozen image |
| `saving failed; QEMU said: ...` | QEMU's migration failed; the guest goes on, on its new overlays |
| `... changed since the checkpoint was saved (...); its disk would not match its memory` | A frozen image, or a file under it, was written, replaced or resized |
| `... has changed since the checkpoint was saved (SHA-256 ...)` | Another QEMU at that path: a checkpoint resumes only with the QEMU that saved it |
| `... is damaged (QEMU's state fails its check)`, `... is truncated` | The stream after the manifest is not as written |
| `loading ... failed: ...` | QEMU could not load the stream, with `info migrate`'s output |

## Limits

- **Components cannot be saved.** After a resume, a component's memory
  refers to its GPU device, textures and event subscriptions by small
  numbers (resource handles), and the table that maps them to host
  objects belongs to the component instance inside wasmtime, with no
  API. wasmtime 41 also exposes none of a component's core instances.
  They can be paused within one process, though: the GUI pauses a
  component while its tab is in the background, with wasmtime's epochs
  ([05-browser-ui.md](05-browser-ui.md#pausing-kind-by-kind)).
- **Threads cannot be saved**: their stacks are always live. The
  multi-threaded QEMU is saved by QEMU's own migration instead.
- **QEMU's checkpoints** hold the guest and its disks, not the world
  outside: the guest's network connections are gone after a resume (the
  host side of its network, `src/qemu_net.rs`, starts afresh), and files
  QEMU writes besides its disks (a `-serial file:` log) are opened anew
  by every copy, at the path in the saved arguments. Read-only disks and
  CD-ROMs are not frozen and not checked. The single-threaded QEMU can
  pause but not save; resuming on it is untested.
- Save points of commands are only the wrapped WASI calls.
- On resume, instantiation runs the module's start function, if it has
  one (a `start` section; compilers rarely emit one), and data and
  element segments the program had dropped (`data.drop`, `elem.drop`)
  are back.
- Standard input and output are m-browser's: input the program has read
  is not given back, and output written before the save is not written
  again. Output still in the program's own buffers is in its memory, so
  it is saved and comes out after the resume.
- A hibernated state can be resumed more than once, as long as the
  program's files have not changed since; once they have, the check
  refuses it.

See also [known-issues.md](known-issues.md).

## Tests

```sh
# from the m-browser directory
cargo test --release --offline -- saved_state wasm_headless qemu_save   # the 28 tests below
```

| Test | Checks |
|---|---|
| `saved_state::tests::names_use_the_basic_utc_form` | UTC times and file names, owner ids |
| `saved_state::tests::sections_round_trip` | Sections, also large streamed ones, read back as written; the final name appears only at the end |
| `saved_state::tests::damage_is_detected` | A flipped byte, a truncated file, a newer version, a foreign file, a damaged header |
| `saved_state::tests::unfinished_files_leave_nothing` | A writer dropped before `finish` leaves no file |
| `saved_state::snapshot::tests::hidden_state_is_exported` | `prepare()` exports hidden memories, tables and mutable globals, with imported globals counted |
| `saved_state::snapshot::tests::existing_exports_and_sections_are_kept` | Exports, a start section and data survive the rewriting |
| `saved_state::snapshot::tests::modules_that_cannot_be_saved_are_recognized` | Threads, table changes, components, taken export names, Asyncify detection |
| `saved_state::snapshot::tests::a_reactor_continues_in_a_new_instance` | Memory (grown, a changed and a zeroed data segment) and a hidden global restored into another engine |
| `saved_state::snapshot::tests::a_damaged_or_foreign_state_is_refused` | A state does not restore into another module |
| `saved_state::snapshot::tests::an_asyncified_command_continues_in_a_new_instance` | Unwound in a nested call, saved, rewound elsewhere: the same result as without a save |
| `saved_state::wasi::tests::files_are_reopened_under_the_same_numbers` | Numbers, offsets, a directory, a file opened in it, and the next number handed out |
| `saved_state::wasi::tests::a_changed_file_is_refused` | A file grown after the save |
| `saved_state::wasi::tests::the_clock_continues_and_stands_still_while_paused` | `ProgramClock` |
| `wasm_headless::tests::a_reactor_hibernates_and_resumes` | A WASI reactor writing to a file, hibernated and resumed: every record, a clock that never goes back |
| `wasm_headless::tests::an_asyncified_command_hibernates_and_resumes` | The same with a command, hibernated twice in one session |
| `wasm_headless::tests::a_failed_save_lets_the_program_go_on` | Both kinds, with an unwritable directory |
| `wasm_headless::tests::a_plain_command_runs_but_cannot_hibernate` | The request is declined, the program finishes |
| `wasm_headless::tests::a_resume_refuses_changed_files_and_modules` | A changed file, then a changed module |
| `wasm_headless::tests::pause_quit_and_unknown_commands` | A program paused before its first step and stopped by `quit` |
| `wasm_headless::tests::options_are_parsed` | The command line |
| `wasm_headless::tests::output_goes_to_the_page_and_the_owner_names_saves` | A program's standard output on an `OutputPage`, a given owner id in the saved state's name, and the resumed run's output on a new page |
| `wasm_headless::tests::a_ticker_prints_to_its_page` | `ticker.wat` prints to its page and stops at its next save point when told to quit |
| `wasm_headless::tests::a_page_keeps_its_last_lines` | An `OutputPage` drops its oldest lines, at a line start, and adds notes on a line of their own |
| `qemu_save::tests::info_block_gives_the_writable_disks` | `info block` output: read-only and empty drives are skipped |
| `qemu_save::tests::drive_arguments_are_rewritten` | `-drive` options with `,,`, finding a disk's argument, removing `-incoming` |
| `qemu_save::tests::paths_map_between_host_and_qemu` | Paths through the preopens, both ways |
| `qemu_save::tests::backing_chains_are_followed` | A qcow2 chain with an absolute and a relative backing file |
| `qemu_save::tests::a_checkpoint_is_checked_before_resuming` | A good checkpoint; then a damaged stream, a `--wasm-headless` file, a changed frozen image, a changed module |

The test programs are WAT (wasm's text form) in
`src/saved_state/testdata/`: `count.wat` and `writer.wat`, and their
Asyncify transforms `count.asyncify.wat` and `writer.asyncify.wat`,
which are generated: the source files say how to make them again.
`ticker.wat` prints a line a second; built into a `.wasm` file
(`wasm-tools parse src/saved_state/testdata/ticker.wat -o ticker.wasm`),
it is also a handy page for trying a core module's tab in the GUI. Tests
write their files under the system's temporary directory (`TMPDIR`) and
remove them.

**Pitfall:** WASI wants 8-byte alignment for 64-bit results (the time
written by `clock_time_get`) and for `poll_oneoff`'s subscriptions, and
refuses to open a directory with write rights (`EISDIR`, errno 31). Test
programs written by hand in WAT trip over these; compilers do not.

Not covered by `cargo test`: programs built by real compilers, in
separate processes. They were checked by hand with the C command and the
C reactor above, and a Rust program built for `wasm32-wasip1` and
transformed with Asyncify, hibernated twice and resumed in three
processes; each time the log file it wrote was complete and in order,
its data in memory intact, and its clock never went back.

QEMU's checkpoints were checked by hand with the multi-threaded build,
booting a root shell from a kernel and initramfs: a checkpoint, the
guest going on after it; two copies resumed at once, both answering,
with `/proc/uptime` going on from the save; a third copy later, which
wrote a file, quit, and whose overlay booted afresh with the file in it;
the frozen images unchanged, and `qemu-img check` clean on every
overlay. The single-threaded build paused and continued, and refused to
save.
