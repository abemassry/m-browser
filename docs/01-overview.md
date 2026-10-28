# Overview

**What this covers.** What m-browser is, what it can run, how it hosts the
QEMU emulator, how it relates to the qemu-wasi repository, and which
chapter to read for which job.  Everyone should read this first; it
assumes no knowledge of Rust, emulators or WebAssembly.  Terms are
explained in [02-concepts.md](02-concepts.md) and in the qemu-wasi
repository's docs/reference/glossary.md.

## In one paragraph

m-browser is a desktop application, written in Rust, that is a small web
browser of a new kind: it shows Markdown pages (instead of HTML) and runs
**WebAssembly** programs (instead of JavaScript), using the **wasmtime**
runtime.  It runs two kinds of WebAssembly programs: graphical apps built
on the wasi-gfx interfaces (for example a Breakout game), and, above all,
**QEMU**, the machine emulator, compiled to WebAssembly by the qemu-wasi
repository, which boots a complete Linux desktop (Alpine Linux with X,
Fluxbox and Chromium) inside a window.  Most of m-browser's code exists to
host QEMU: it gives QEMU the services a WebAssembly program cannot provide
for itself.

## The picture

```text
 m-browser process
 +--------------------------------------------------------------------------+
 | UI thread: winit window, egui widgets (tabs, address bar, Markdown)      |
 |            child window showing the guest's screen; keyboard and mouse   |
 |                         |  events                  ^  frames             |
 |                         v                          |                     |
 | QEMU thread: wasmtime Store running qemu-system-x86_64.wasm             |
 |   QEMU calls m-browser ("host imports"):                                 |
 |     JIT ............ compile QEMU's generated code modules  (qemu_jit)   |
 |     display/input .. frames out, events in                  (qemu_gfx)   |
 |     network ........ Ethernet frames <-> host sockets       (qemu_net)   |
 |     threads ........ one thread per guest CPU               (qemu_threads)|
 |     WASI ........... files (m.qcow2, pc-bios), clocks, random            |
 |                                                                          |
 | helper threads: background compilers, network, QEMU's vCPU threads      |
 +--------------------------------------------------------------------------+
        |                   |                       |
     m.qcow2           the screen              the internet
```

## What it can do

| Feature | Where | Chapter |
|---|---|---|
| Show Markdown pages, follow links, tabs | `src/app.rs` | [05-browser-ui.md](05-browser-ui.md) |
| Run wasi-gfx WebAssembly components | `src/wasm.rs` (`Wasm`), `src/winit_wasi.rs` | [05-browser-ui.md](05-browser-ui.md) |
| Run QEMU in a window | `src/app.rs`, `src/wasm.rs` | [06-qemu-runner.md](06-qemu-runner.md) |
| Compile QEMU's generated code (JIT) | `src/qemu_jit.rs` | [07-jit-host.md](07-jit-host.md) |
| Run QEMU's guest CPUs on threads | `src/qemu_threads.rs` | [08-threads.md](08-threads.md) |
| Show the guest's screen; keyboard and mouse | `src/qemu_gfx.rs`, `src/qemu_keymap.rs` | [09-display-and-input.md](09-display-and-input.md) |
| Give the guest a network (DHCP, DNS, UDP, TCP, ping, port forwarding) | `src/qemu_net.rs`, `src/qemu_net/tcp.rs` | [10-networking.md](10-networking.md) |
| Run QEMU without a window, scripted | `src/headless.rs` | [11-headless-and-automation.md](11-headless-and-automation.md) |
| Profile QEMU | `src/qemu_profile.rs` | [12-profiling.md](12-profiling.md) |

## How it relates to qemu-wasi

```text
 qemu-wasi repository                          m-browser repository
 +--------------------------+   make install    +-----------------------------+
 | QEMU 9.1 + patches       | ----------------> | qemu-system-x86_64.wasm     |
 | -> qemu-system-x86_64    |                   | (tracked in git)            |
 +--------------------------+                   |                             |
              ^                                 | src/: the host side of the  |
              |   the host interface:           | same interface              |
              +-- functions QEMU imports -------+                             |
                  and exports                   +-----------------------------+
```

- qemu-wasi builds QEMU and owns the QEMU side of the interface; m-browser
  owns the host side.  When the interface changes, both change together:
  a QEMU build that imports a function m-browser lacks fails to start.
- The interface is listed in
  [reference/host-imports.md](reference/host-imports.md) (and in the
  qemu-wasi repository's docs/reference/host-interface.md).
- m-browser ships the multi-threaded QEMU build; it also runs the
  single-threaded one (it recognizes which by its imports).

## How fast is it

With the multi-threaded QEMU on a 20-thread laptop: the guest's desktop
after about 2.5 minutes, Chromium's window after about 4, and clicks in
Chromium answered in 6-9 seconds.  The qemu-wasi repository's
docs/15-performance.md has the measurements and the tools.

## Reading paths

| I want to... | Read |
|---|---|
| Learn the Rust and WebAssembly I need | [02-concepts.md](02-concepts.md) |
| Build and run it | [03-getting-started.md](03-getting-started.md) |
| Find my way around the code | [04-architecture.md](04-architecture.md) |
| Change the browser UI | [05-browser-ui.md](05-browser-ui.md) |
| Change how QEMU is started, stopped or kept in memory | [06-qemu-runner.md](06-qemu-runner.md) |
| Work on the JIT or threads | [07-jit-host.md](07-jit-host.md), [08-threads.md](08-threads.md) |
| Work on the screen, keyboard, mouse | [09-display-and-input.md](09-display-and-input.md) |
| Work on the network | [10-networking.md](10-networking.md) |
| Script QEMU or measure it | [11-headless-and-automation.md](11-headless-and-automation.md), [12-profiling.md](12-profiling.md) |
| Add a feature | [13-adding-features.md](13-adding-features.md) |
| Test or debug | [14-testing-and-debugging.md](14-testing-and-debugging.md), [known-issues.md](known-issues.md) |
| Look up a setting or an import | [reference/environment-variables.md](reference/environment-variables.md), [reference/host-imports.md](reference/host-imports.md) |
