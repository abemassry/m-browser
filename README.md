# The M Browser

## Overview

The web and the web browser had a lot of good ideas. Maybe we can take some of those good ideas
and combine it with the utility of the web that puts the power of the web back in the hands of the user.

## Features

- **Display Markdown**: Display markdown files by default.
- **JSON format**: Display Markdown and or wasm files if specified in a json file. (planned
  feature)
- **Display WASM**: Display wasm files in a surface that renders by default.
- **Legacy Webpages**: Display legacy webpages in a container that runs a full system including a
  browser, through wasm, that renders to a surface: QEMU compiled to WebAssembly runs an Alpine
  Linux desktop with Chromium (see below).

## A Linux desktop in WebAssembly

Open `file:///<path to m-browser>/qemu-system-x86_64.wasm` to boot the Alpine Linux desktop in
`m.qcow2` (not in git) with QEMU, built for WebAssembly by the
[qemu-wasi](https://github.com/abemassry/qemu-wasi) repository. Build m-browser with
`cargo build --release` and start it from its own directory: QEMU finds its firmware
(`pc-bios/`) and the disk image there.

## Documentation

The developer documentation starts at [docs/README.md](docs/README.md): an overview, getting
started, the architecture, and one chapter per part (the QEMU runner, the JIT, threads, display
and input, networking, headless automation, profiling), plus recipes for adding features and a
testing and debugging guide.

## Example inside the M Browser

[Link to second page in example](https://raw.githubusercontent.com/abemassry/m-browser/refs/heads/main/pages/SECOND_PAGE.md)

[Link to wasm example](https://wsnd.io/i0cyOnP4/basic_triangle.wasm)

## Screenshots
Example of markdown display:
![screenshot of M Browser displaying markdown](https://raw.githubusercontent.com/abemassry/m-browser/refs/heads/main/pages/screenshot_20251020.png)

Example of wasm display:
![screenshot of M Browser displaying wasm](https://raw.githubusercontent.com/abemassry/m-browser/refs/heads/main/pages/screenshot_wasm_20251020.png)

Example of obligatory meta screenshot:
![screenshot of M Browser displaying itself](https://raw.githubusercontent.com/abemassry/m-browser/refs/heads/main/pages/screenshot_meta_20251020.png)
