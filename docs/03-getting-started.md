# Getting started

**What this covers.** How to get from a fresh Linux machine to an Alpine
Linux desktop running inside m-browser: what to install, getting the code
and the disk image, building and starting the browser, running QEMU in a
window or headless, installing a new QEMU build, running the tests, and
fixing a failed first run. No Rust knowledge is assumed; Rust terms are
explained where they come up, and further in
[02-concepts.md](02-concepts.md). Building QEMU itself is described in the
qemu-wasi repository's docs/03-getting-started.md.

## In a hurry

On Debian or Ubuntu, in a graphical desktop session (package names are
typical; the path of the disk image is a placeholder):

```sh
# any directory (accept rustup's defaults)
sudo apt install build-essential pkg-config libssl-dev git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"
# the directory that will hold the project, e.g. ~/src
git clone -b qemu-wasi https://github.com/abemassry/m-browser.git
cd m-browser
cp /path/to/team-share/m.qcow2 .        # the disk image, about 2 GB
cargo build --release
./target/release/m-browser
```

In the window, click the address bar, replace its text with
`file:///<absolute path of m-browser>/qemu-system-x86_64.wasm` and press
Enter. QEMU's screen appears inside the browser; the Alpine desktop follows
after about 2.5 minutes.

## Requirements

| What | Why | How to check | How to install (Debian/Ubuntu; package names typical) |
|---|---|---|---|
| x86-64 Linux with an X11 or Wayland desktop session | m-browser opens native windows; the multi-threaded QEMU orders memory correctly only on x86-64 hosts ([08-threads.md](08-threads.md)) | `uname -m` prints `x86_64`; `echo $XDG_SESSION_TYPE` prints `x11` or `wayland` | - |
| 8+ hardware threads, 8 GB+ RAM, ~10 GB free disk (estimates) | 8 or more threads give the guest 4 virtual CPUs instead of 2; QEMU alone can use 2 GB or more; the image takes 2 GB, `target/` about 3 GB, `~/.cargo` 2-3 GB | `nproc`, `free -g`, `df -h .` | - |
| Rust 1.91 or newer, through rustup | Compiler and build tool (below) | `rustc --version` | The `curl ... \| sh` line above; later `rustup update stable` |
| A C compiler and linker | rustc links with `cc`; ring, openssl-sys, zstd-sys and wasmtime compile some C | `cc --version` | `build-essential` |
| pkg-config and OpenSSL's development files | reqwest's default TLS backend is native-tls, which is OpenSSL on Linux (`openssl-sys` in Cargo.lock; the binary links `libssl.so.3`) | `pkg-config --modversion openssl` prints a version | `pkg-config libssl-dev` |
| git, and network access for the first build | Cloning; Cargo downloads crates from crates.io and three wasi-gfx crates from github.com | `git --version` | `git` |
| X11 or Wayland client libraries (run time) | winit and softbuffer load them with `dlopen` when the window opens, so a missing one shows up at run time, not at build time (`ldd target/release/m-browser` lists only `libssl`, `libcrypto`, `libgcc_s`, `libm`, `libc`) | `ldconfig -p \| grep -E 'libxkbcommon\|libX11-xcb\|libwayland-client'` | Usually preinstalled. X11: `libx11-6 libx11-xcb1 libxcursor1 libxi6 libxcb1 libxkbcommon0 libxkbcommon-x11-0`; Wayland: `libwayland-client0 libxkbcommon0` |
| A Vulkan or OpenGL (EGL) driver (run time) | wgpu draws the browser's UI on the GPU; it loads `libvulkan.so.1` or `libEGL.so.1` the same way | `vulkaninfo --summary` (package `vulkan-tools`) names your GPU | Intel, AMD: `libvulkan1 mesa-vulkan-drivers`; NVIDIA: its proprietary driver; OpenGL: `libegl1 libgl1-mesa-dri` |
| The disk image `m.qcow2` | The guest's disk; not in git | `ls -l m.qcow2` | The team's shared storage ([below](#getting-the-code-and-the-disk-image)) |

**Rust.** *rustup* installs Rust toolchains in your home directory
(`~/.rustup`, commands in `~/.cargo/bin`); *cargo* is Rust's build tool
and package manager; a *crate* is a package. Distribution packages of
`rustc` are usually too old. Why 1.91: Cargo.lock pins smoltcp 0.14.0,
which declares `rust-version = "1.91"` (wasmtime 41.0.4: 1.90), and Cargo
refuses older compilers; `File::try_lock()` in `lock_disk_image()`
(`src/app.rs`) alone would need 1.89. The last build used rustc 1.91.1
(`target/.rustc_info.json`). Newer releases should work but are untried;
if one fails, run `rustup toolchain install 1.91.1`, then build with
`cargo +1.91.1 build --release`.

## Getting the code and the disk image

1. Clone m-browser on its `qemu-wasi` branch, which holds the QEMU support
   (`main` does not). Clone qemu-wasi next to it only if you will rebuild
   QEMU: its `make install` expects m-browser in `../m-browser`.

   ```sh
   # the parent directory, e.g. ~/src
   git clone -b qemu-wasi https://github.com/abemassry/m-browser.git
   git clone https://github.com/abemassry/qemu-wasi.git    # optional
   ```

   With an SSH key on GitHub, `git@github.com:abemassry/...` works too.
2. The clone already contains a QEMU build: `qemu-system-x86_64.wasm`
   (about 40 MB, the multi-threaded build, tracked in git) and its firmware
   in `pc-bios/`.
3. Copy the disk image from the team's shared storage (ask the team for
   its current location) into the m-browser directory. The guest writes to
   it as it runs; if your copy gets damaged, copy the original again. It
   is an Alpine Linux image with Xorg, the Fluxbox window manager and
   Chromium, which logs in automatically (qemu-wasi's
   docs/16-guest-image.md). Name and place are fixed (`QEMU_DISK_IMAGE` in
   `src/app.rs`, QEMU's `-drive` option); `.gitignore` keeps it out of git.

   ```sh
   # from the m-browser directory
   cp /path/to/team-share/m.qcow2 .
   chmod u+rw m.qcow2     # QEMU writes to it
   ls -l m.qcow2          # about 2 GB
   ```

## Building

```sh
# from the m-browser directory
cargo build --release              # the first time: needs the network
cargo build --release --offline    # afterwards
```

The program is `target/release/m-browser`. The first build downloads the
crates pinned in Cargo.lock (about 660 packages, some only for other
platforms) and revision `73105bc0...` of the wasi-gfx-runtime repository,
then compiles everything: about 3 to 4 minutes on the 20-thread development
machine (estimated from file times in `target/`, not timed), several times
that on a laptop. After you edit `src/`, only m-browser itself recompiles:
30 to 45 seconds there (estimated the same way). `--offline` works once
everything is cached under `~/.cargo`, and fails when Cargo.toml or
Cargo.lock asks for something new: build once without it then.

**Always build with `--release`.** Without it, Cargo makes an unoptimized
*debug build* in `target/debug/`, libraries included: Cranelift, wasmtime's
compiler, which keeps compiling while QEMU runs (QEMU's module, then every
piece of guest code that runs often), and the JIT host, network and display
code, which run for every code block, packet and frame. Unoptimized Rust is
typically many times slower, so QEMU becomes far too slow to use (not timed
for this guide).

## Running the browser

```sh
# from the m-browser directory; tee keeps a copy of the log
./target/release/m-browser 2>&1 | tee /tmp/m-browser.log
```

**Pitfall:** always start it from the m-browser directory. It opens
`./m.qcow2` and `./pc-bios` relative to the working directory and writes
`./downloaded.wasm` there (a copy of the last `.wasm` page; gitignored).

A window titled "M" opens (it asks for 3840x2160 pixels; window managers
usually shrink it). At the top: a menu (theme, "Quit"), back, forward, the
address bar and Go (`>>`); on the left the tabs and `+`; at the bottom the
status. The address bar shows the project README on
raw.githubusercontent.com, but nothing is fetched until you press Enter or
Go: until then the page is empty and the status says "Loading...". Local
Markdown works too, e.g. `file:///<path of m-browser>/pages/SECOND_PAGE.md`
([05-browser-ui.md](05-browser-ui.md)). Pages load on the UI thread, so
the window freezes until the server answers.

## Running QEMU

1. Start the browser as above.
2. Click the address bar, select its text (Ctrl+A) and type or paste
   `file://` followed by the absolute path of `qemu-system-x86_64.wasm`
   (hence three slashes). In the m-browser directory,
   `echo "file://$PWD/qemu-system-x86_64.wasm"` prints it.
3. Press Enter (or Go). m-browser copies the module to `downloaded.wasm`,
   opens a 1080x720 child window inside the main window, right of the tabs
   and below the toolbar, and starts QEMU on a thread of its own ([its
   arguments](05-browser-ui.md#from-the-address-bar-to-a-running-qemu)).

What to expect on the development machine (20 hardware threads, the
multi-threaded QEMU with 4 virtual CPUs):

| After Enter | QEMU's window | Terminal |
|---|---|---|
| At once | Empty | `[detect] ... is_core=true`, `[qemu] [...] Engine created` |
| First start of a QEMU build only: about 1 s here (reported, not re-measured), longer on smaller machines | Empty | `compiling QEMU's module with optimizations (done once, then cached)...`, then `compiled in N s; caching it in .../m-browser/qemu-<hash>.cwasm` |
| Seconds later | Text-mode boot screens (firmware, boot loader, Linux) | `Multi-threaded QEMU: one thread per vCPU`, `4 vCPUs (QEMU_SMP)` (2 on hosts with fewer than 8 CPUs), `Running`, `[qemu-gfx] host_gfx_init(...)` |
| About 2.5 minutes | The Alpine desktop (Fluxbox) | |
| About 4 minutes | Chromium's window (the image starts it after login) | |

The compiled module is cached in `$XDG_CACHE_HOME/m-browser/`, by default
`~/.cache/m-browser/`, as `qemu-<hash>.cwasm` (about 57 MB; the 3 newest
are kept), so later starts skip the compilation. Settings such as the
number of virtual CPUs are environment variables, e.g.
`QEMU_SMP=2 ./target/release/m-browser`
([reference/environment-variables.md](reference/environment-variables.md)).

**Note:** the child window keeps its size, 1080x720 logical pixels. The
guest's X screen is 1280x800, so on a display with scale factor 1 its right
and bottom edges are cut off ([details](09-display-and-input.md#pitfalls)).

### Keyboard and mouse

Keys go to QEMU whenever its window is open and no browser text field has
the keyboard focus (`qemu_takes_keys` in `App::window_event()`,
`src/app.rs:709-714`), so you need not click into QEMU's window: Enter in
the address bar releases the focus, and so do Escape and a click outside
it. The mouse reaches QEMU while the pointer is over its window, and the
guest's pointer follows yours exactly (QEMU emulates a USB tablet).

| Keys | Shows |
|---|---|
| Ctrl+Alt+1 | The guest's screen |
| Ctrl+Alt+2 | QEMU's monitor, a `(qemu)` prompt: try `info status`, `info network` |
| Ctrl+Alt+3 | The guest's serial port |

QEMU consumes these, so the guest never sees them; your desktop may grab
others (Alt+Tab, Super, ...) before m-browser sees them.

### Stopping QEMU

| You | What happens |
|---|---|
| Close the main window, or menu, "Quit" | m-browser asks QEMU to quit and waits up to 5 s; QEMU flushes and closes `m.qcow2`. For the guest, a power cut |
| Open another address | The same, without waiting; the child window goes away |
| `poweroff` as root in the guest | A clean shutdown; QEMU exits (`[qemu] exited with status 0`); the child window stays until you navigate |
| Ctrl+C in the terminal, `kill` | Avoid: `m.qcow2` is not closed and may be damaged |

After a power cut the guest's file systems replay their journals and
Chromium may offer to restore its pages; both are normal. To check the
image, with m-browser closed: `qemu-img check m.qcow2` (`qemu-utils`).

## Running QEMU headless

`--qemu-headless` runs QEMU without a window, from a terminal or a script
([11-headless-and-automation.md](11-headless-and-automation.md)). It takes
no disk lock: give it a copy of the image, never one a GUI may be using.

```sh
# from the m-browser directory
mkdir -p ~/vm-test && cp m.qcow2 ~/vm-test/disk.qcow2
QEMU_SNAPSHOT=$HOME/vm-test/screen.png QEMU_QUIT_AFTER=300 \
./target/release/m-browser --qemu-headless --preopen "$HOME/vm-test=/out" \
    qemu-system-x86_64.wasm -L /bios \
    -drive file=/out/disk.qcow2,format=qcow2 -m 1024 \
    -accel tcg,tb-size=256 -device qemu-xhci -device usb-tablet \
    -monitor vc -serial vc -nic user
```

The module comes first, then QEMU's arguments: the GUI's, with the copy as
the disk (`--preopen` makes `~/vm-test` visible to QEMU as `/out`).
`screen.png` shows the screen about once a second; QEMU quits cleanly after
300 s, well after the desktop has appeared.

## Updating QEMU

QEMU is built in the qemu-wasi repository, which needs wasi-sdk, ninja and
python3 among others (its docs/03-getting-started.md). `make install`
copies the result over m-browser's `qemu-system-x86_64.wasm`:

```sh
# from the qemu-wasi directory, next to m-browser
make THREADS=1 install    # the multi-threaded build, the one m-browser ships
make install              # or the single-threaded build
```

Both write the same file (`M_BROWSER=/path/to/m-browser` changes where).
Then press Enter on QEMU's address again, which replaces the running QEMU,
or restart the browser. The cache key hashes the module's bytes and
wasmtime's settings, so a new build compiles once on its first start, as
does the old one after a wasmtime or Rust update. The `.wasm` is tracked
in git: commit it with the m-browser changes it needs, since both sides of
the [host interface](reference/host-imports.md) must match.

## Running the tests

```sh
# from the m-browser directory
cargo test --release --offline qemu_net
```

This builds a test version of m-browser (slow the first time) and runs its
11 unit tests, all for the guest's network; they need no network access
and no privileges, and end with `test result: ok. 11 passed`. There is no
automated GUI test: before committing anything that reaches QEMU, do the
[smoke test](14-testing-and-debugging.md#smoke-test-of-the-gui). QEMU's
instruction test (`make test`) is in qemu-wasi.

## Troubleshooting the first run

| Symptom | Cause | Fix |
|---|---|---|
| ``linker `cc` not found`` | No C toolchain | `sudo apt install build-essential` |
| The build fails in `openssl-sys` (`failed to run custom build command`) | pkg-config or OpenSSL's development files missing | `sudo apt install pkg-config libssl-dev` |
| Cargo reports a package that `requires rustc 1.91` | Rust too old, often the distribution's | `rustup update stable`; `command -v cargo` should print `~/.cargo/bin/cargo` |
| Panic: `neither WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is set` | No graphical session (over SSH, say) | Start it in the desktop session, or use the headless mode |
| Panic: `Failed to find an appropriate adapter` or `Failed to create device` | No usable Vulkan or OpenGL driver | Install or repair the GPU driver; `vulkaninfo --summary` should list the GPU |
| Panic: `failed to select proper surface texture format!` | `AppState::new()` (`src/app.rs`) requires a `Bgra8UnormSrgb` surface, which the driver does not offer; likeliest on wgpu's OpenGL fallback (not verified) | Get Vulkan working; without a GPU, Mesa's software Vulkan driver in `mesa-vulkan-drivers` may do (untested) |
| The browser closes when you press Enter on a `file://` address; a panic with `NotFound` | The file does not exist: a typo, `~`, a relative path, or no `file://` | For QEMU, paste the output of `echo "file://$PWD/qemu-system-x86_64.wasm"` |
| `Error running QEMU core module: No such file or directory (os error 2)` after `Preopening: pc-bios -> /bios` | Started from another directory | `cd` into m-browser and start it again |
| `Could not open '/data/m.qcow2'`, then `QEMU exited during startup with status 1` | No `m.qcow2` in the m-browser directory, or no permission | Copy it there; `chmod u+rw m.qcow2` |
| `m.qcow2 is in use by another QEMU; not starting a second one on the same disk image` | Another m-browser runs QEMU on this image | Close it; `fuser -v m.qcow2` finds it |
| QEMU's window stays black | QEMU did not start (read the terminal); a text console is shown; the guest blanked its screen after 10 idle minutes | Fix what the log says; Ctrl+Alt+1; move the mouse over the window or press a key |
| Keys go to the address bar, not the guest | The address bar has the keyboard focus | Press Escape or click outside it |
| QEMU shows up in a window of its own | Wayland session: winit cannot embed child windows there (its documentation; untested) | `env -u WAYLAND_DISPLAY ./target/release/m-browser` runs it through XWayland |
| Pings to the internet fail; `[qemu-net] cannot ping from the host: ...` | Your group is not in `net.ipv4.ping_group_range` (`sysctl net.ipv4.ping_group_range`) | `sudo sysctl -w net.ipv4.ping_group_range="0 2147483647"` (until reboot). TCP, UDP and pings to 10.0.2.2 work regardless |
| The desktop takes far longer than 2.5 minutes | A debug build, the first start of a new QEMU, the single-threaded build, fewer CPUs | "Slow boot" in [14-testing-and-debugging.md](14-testing-and-debugging.md#symptoms-causes-and-fixes) |

## Next steps

Read in this order ([README.md](README.md) lists every document):

1. [01-overview.md](01-overview.md), [02-concepts.md](02-concepts.md) (the
   Rust, WebAssembly and wasmtime needed to read the code) and
   [04-architecture.md](04-architecture.md).
2. The subsystem you will work on: [05-browser-ui.md](05-browser-ui.md),
   [06-qemu-runner.md](06-qemu-runner.md), [07-jit-host.md](07-jit-host.md),
   [08-threads.md](08-threads.md),
   [09-display-and-input.md](09-display-and-input.md),
   [10-networking.md](10-networking.md).
3. Tools: [11-headless-and-automation.md](11-headless-and-automation.md),
   [12-profiling.md](12-profiling.md),
   [14-testing-and-debugging.md](14-testing-and-debugging.md); before you
   change the interface to QEMU, [13-adding-features.md](13-adding-features.md).
4. [known-issues.md](known-issues.md), then
   [reference/environment-variables.md](reference/environment-variables.md)
   and [reference/host-imports.md](reference/host-imports.md).

On the QEMU side: the qemu-wasi repository's docs/03-getting-started.md
(building QEMU) and docs/16-guest-image.md (the disk image).
