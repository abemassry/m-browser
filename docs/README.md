# m-browser documentation

m-browser is a desktop browser for Markdown pages and WebAssembly
programs, written in Rust.  Its biggest job is hosting QEMU, compiled to
WebAssembly by the qemu-wasi repository, to run a Linux desktop in a
window.  These documents start with a high-level overview and go down to
the details.  They are written for developers who know general programming
but not Rust, emulators or WebAssembly.

Start with [01-overview.md](01-overview.md).

## Start here

| Chapter | What it covers |
|---|---|
| [01-overview.md](01-overview.md) | What m-browser is and does; reading paths for each job |
| [02-concepts.md](02-concepts.md) | Primer: the Rust, wasmtime and WebAssembly needed to read this code |
| [03-getting-started.md](03-getting-started.md) | From a fresh machine to a running desktop in m-browser |
| [04-architecture.md](04-architecture.md) | The map: files, threads, data flows, design decisions |

## How it works

| Chapter | What it covers |
|---|---|
| [05-browser-ui.md](05-browser-ui.md) | The window, the Markdown browser, starting and stopping QEMU |
| [06-qemu-runner.md](06-qemu-runner.md) | Loading QEMU, driving it step by step, moving it to fresh memory |
| [07-jit-host.md](07-jit-host.md) | Compiling QEMU's generated code: caches, background compilers |
| [08-threads.md](08-threads.md) | Hosting the multi-threaded QEMU: threads, Stores, shared state |
| [09-display-and-input.md](09-display-and-input.md) | Frames from QEMU to the window; keyboard and mouse to QEMU |
| [10-networking.md](10-networking.md) | The guest's virtual network: DHCP, DNS, UDP, TCP, ping, port forwarding |
| [11-headless-and-automation.md](11-headless-and-automation.md) | Running QEMU without a window, typing and clicking from scripts |
| [12-profiling.md](12-profiling.md) | The sampling profiler |

## Changing it

| Chapter | What it covers |
|---|---|
| [13-adding-features.md](13-adding-features.md) | Adding host functions and settings; worked example: UDP port forwarding |
| [14-testing-and-debugging.md](14-testing-and-debugging.md) | Tests, smoke tests, debugging techniques, symptoms and fixes |
| [known-issues.md](known-issues.md) | Known bugs and limitations |

## Reference

| Document | What it covers |
|---|---|
| [reference/host-imports.md](reference/host-imports.md) | Every function m-browser provides to QEMU, and every QEMU export it calls |
| [reference/environment-variables.md](reference/environment-variables.md) | Every environment variable m-browser reads |

QEMU's side (building it, its patches, its code generator, upgrading it,
the guest disk image, performance work and the project's history) is
documented in the qemu-wasi repository's `docs/` directory.
