# Rust, wasmtime and WebAssembly primer for this codebase

**What this covers.** The minimum of Rust, of the GUI libraries, of
WebAssembly and of wasmtime that you need to read m-browser's source, each
concept tied to the place where the code uses it. Read it if you are
comfortable with general programming (web or backend work, say) but new to
Rust, emulators or WebAssembly. It is not a tutorial; the last section
points to the real documentation. Emulator terms (TB, TCG, vCPU, ...) are
explained in the qemu-wasi repository's docs/reference/glossary.md.

## The whole picture

```text
m-browser process (native code compiled from Rust)
+-----------------------------------------------------------------------+
| main thread: winit event loop -> egui UI -> wgpu (GPU) -> main window |
|                                                                       |
| QEMU thread(s): wasmtime                                              |
|   Engine (compiler settings) + Store<QemuHostState> (one per thread)  |
|                                                                       |
|   +----------------------------+  imports = host functions:           |
|   | QEMU core module (wasm)    |--> wasi_snapshot_preview1.*          |
|   |   linear memory            |      (wasmtime-wasi: files, clocks)  |
|   |   function table           |--> env.wasi_jit_*   src/qemu_jit.rs  |
|   | TB modules (wasm, compiled |--> env.host_gfx_*   src/qemu_gfx.rs  |
|   |   while QEMU runs)         |--> env.host_net_*   src/qemu_net.rs  |
|   +----------------------------+                                      |
+-----------------------------------------------------------------------+
```

A TB (translation block) is a short piece of the emulated PC's machine
code that QEMU translated in one go; in this project each one becomes a
small wasm module that the host compiles while QEMU runs.

**Note:** two words have two meanings in this code. For QEMU, the *guest*
is the emulated PC (Alpine Linux) and the *host* is the real computer. For
wasmtime, QEMU itself is the guest and m-browser is the host. So
`QemuHostState::guest_memory` and `with_guest()` in `src/qemu_jit.rs` mean
*QEMU's* WebAssembly memory (which in turn contains the emulated PC's RAM),
and "host functions" are the Rust functions m-browser gives to QEMU.

## Rust

### Cargo, crates and Cargo.lock

Cargo is Rust's build tool and package manager. A *crate* is a package (a
library or a program). `Cargo.toml` lists the dependencies with version
ranges; `Cargo.lock` (committed) records the exact versions used, so every
build uses the same code (for example wasmtime 41.0.4 and winit 0.30.12).
A *feature* switches on an optional part of a crate, for example wasmtime's
`call-hook` (needed by the profiler and the threads code) or reqwest's
`blocking`.

All dependencies come from crates.io except three wasi-gfx crates
(`wasi-graphics-context-wasmtime`, `wasi-surface-wasmtime`,
`wasi-webgpu-wasmtime`), which are *git dependencies*: Cargo.toml pins them
to revision `73105bc070e40699b32ee198c6c5fc449b1d2640` of the
wasi-gfx-runtime repository.

Build from the m-browser directory:

```sh
cargo build --release --offline    # produces target/release/m-browser
```

`--offline` works because every crate is already downloaded under
`~/.cargo` (crates.io packages in the registry cache, the wasi-gfx crates
as git checkouts). It fails as soon as Cargo.toml asks for a crate or
version that is not in that cache; build once with network access then.

**Note:** `lock_disk_image()` in `src/app.rs` uses `File::try_lock()`,
which needs Rust 1.89 or newer.

**Note:** not every crate in Cargo.toml is used by `src/` any more (tokio,
env_logger, log, wit-bindgen, async-broadcast, callback-future, bytemuck
and async-trait are not referenced). Check with a build before removing
them.

### Modules

`mod app;` in `src/main.rs` compiles `src/app.rs` as the module `app`; all
top-level modules are declared there. `crate::qemu_gfx::InputEvent` names
an item by its path from the crate root. `src/qemu_net.rs` declares
`mod tcp;`, which is `src/qemu_net/tcp.rs`. `#[path = "fill.rs"] mod fill;`
in `src/app.rs` makes `fill.rs` a sub-module of `app`. `pub` makes an item
visible outside its module, `pub(crate)` only inside this crate (for
example `MIGRATE_AFTER_INSTANCES` in `src/wasm.rs`). `#[cfg(...)]` compiles
code only under a condition: `#[cfg(test)]` marks unit tests (in
`src/qemu_net.rs`), and `src/fill.rs` depends on a Cargo feature that is
never enabled.

### Ownership in one paragraph

Every value has exactly one owner, and when the owner goes out of scope the
value is *dropped*: its memory is freed, files are closed, locks are
released. Assigning or passing a value *moves* it (the old variable is no
longer usable); `&x` lends it for reading and `&mut x` lends it exclusively
for writing, and the compiler rejects code where a loan could outlive the
owner or a `&mut` coexists with another loan. In this code that shows up
as: `drop(x)` to end a value early (`drop(gfx)` in `host_gfx_poll_events`
releases the display lock before QEMU's memory is written);
`store.into_data()` in `migrate_store()` consuming the old Store, which is
what frees thousands of compiled-code instances; `.clone()` on an `Arc`
copying a pointer, not the data; and guards kept alive by name until the
end of a scope (`_image_lock` holds the disk-image lock, `_timer` times a
host call, `_stop_profiler` prints the last profile when QEMU stops).

**Pitfall:** `let _ = lock_disk_image(...)` would drop the lock
immediately; `let _image_lock = ...` keeps it. A name starting with `_` is
kept, the bare `_` is not.

### Threads and sharing data between them

| Tool | What it is | Where it is used |
|---|---|---|
| `std::thread::spawn`, `thread::Builder` | Start an OS thread running a closure. The `JoinHandle` can `join()` (wait) or report `is_finished()` | QEMU's thread in `src/app.rs`; vCPU threads in `src/qemu_threads.rs`; compiler threads in `src/qemu_jit.rs`; the profiler's ticker |
| `Arc<T>` | Shared ownership, thread-safe reference counting | `Arc<Mutex<QemuGfxState>>`, the stop flag `Arc<AtomicBool>`, `Arc<Window>` |
| `Mutex<T>` | A lock around data; `lock()` returns a guard that unlocks when dropped. `lock().unwrap()` panics if a thread panicked while holding it | `GfxEventQueue`, `JitCache` |
| `AtomicBool`, `AtomicU64`, ... | Integers and flags shared without a lock; `Ordering::Relaxed` is enough for a flag that is polled | `QemuRun::stop`, the profiler's `TICK` |
| `mpsc` channels | A queue from senders to one receiver | stop signals of the component path (`App::sender`, `event_sender`); jobs for the JIT compiler threads |
| `Condvar` | Sleep until another thread signals | `host_net_wait` in `src/qemu_net.rs` |
| `OnceLock`, `thread_local!` | A global set once; a global per thread | settings read from the environment; the profiler's state |
| `UnsafeCell` + `unsafe impl Sync` | Shared data whose locking the programmer guarantees | `SharedWasi` in `src/qemu_threads.rs` |

The compiler only lets a value cross threads if its type is `Send` (and be
shared if it is `Sync`); these are marker traits the compiler checks, which
is why shared things are wrapped in `Arc` and `Mutex`.

### Closures

`|a, b| a + b` is an anonymous function that can use variables of the
surrounding code; `move |...|` takes ownership of them. Closures are
everywhere: the UI is built inside closures (`panel.show(ctx, |ui| ...)`),
threads run one, and every host function is one:

```rust
linker.func_wrap("env", "host_gfx_resize",
    |caller: Caller<'_, QemuHostState>, width: i32, height: i32| {
        /* ... */
    })?;
```

wasmtime derives the WebAssembly signature from the closure's parameter
and return types (`i32`/`u32` are wasm `i32`, `i64` is `i64`, ...). If
they do not match what QEMU imports, instantiation fails with an error
naming the import. A closure that needs shared state captures a clone of
an `Arc` with `move` (the event queue in `add_gfx_to_linker()`).

### Errors

`Result<T, E>` is either `Ok(value)` or `Err(error)`; `Option<T>` is
`Some(value)` or `None`. `?` returns early with the error. The QEMU code
uses the `anyhow` crate: `anyhow::Result<T>` holds any error,
`.context("...")` and `.with_context(|| ...)` wrap it in a message,
`bail!(...)` returns a new error, and `e.downcast_ref::<T>()` asks whether
it is a particular error, such as `wasmtime_wasi::I32Exit` (QEMU called
`exit()`). Printed with `{e:#}` an error shows its whole chain of messages
on one line (the headless runner does this). wasmtime's own errors are
`anyhow` errors too.

`unwrap()` and `expect()` turn an error into a *panic*, which ends the
thread; on the UI thread that ends the browser and the QEMU in it. A few
remain in `src/app.rs` (opening a missing local file, for example).
`std::panic::catch_unwind` in `App::window_event()` contains one known
panic of the wasi-gfx runtime (see [05-browser-ui.md](05-browser-ui.md)).

### unsafe

An `unsafe { ... }` block allows operations the compiler cannot check
(raw pointers, functions with preconditions); the programmer guarantees
them. Every use in m-browser:

| Where | What | What must hold |
|---|---|---|
| `spawn_child_window()`, `src/app.rs` | `with_parent_window()` | The parent's raw handle stays valid (the main window lives as long as the app) |
| `load_qemu_module()`, `src/wasm.rs` | `Module::deserialize_file()`, `Module::deserialize()` | The bytes are machine code wasmtime produced; it cannot verify them. Only the cache written by `precompile_module()` is loaded |
| `nonzero_runs()`, `src/wasm.rs` | `align_to::<u64>()` | Any bit pattern is a valid `u64` |
| `with_guest()`, `with_guest_mut()`, `src/qemu_jit.rs` | `slice::from_raw_parts(_mut)` over shared memory | Other threads may write meanwhile; callers accept a torn read, and QEMU does not touch buffers it hands to the host |
| `SharedWasi`, `src/qemu_threads.rs`; `QemuHostState::wasi_ctx()` | `unsafe impl Sync`, `UnsafeCell::get()` | Only the thread holding the `locked` spin lock uses the WASI context |
| `src/fill.rs` | `mem::transmute` | Compiled out (the feature is never enabled) |

## The GUI stack

| Crate | Role | In this code |
|---|---|---|
| winit | Creates native windows and delivers events (keys, mouse, resize, redraw) | `App` implements `winit::application::ApplicationHandler`; `main()` runs `EventLoop::run_app()` |
| egui | *Immediate-mode* UI: code describes the whole UI again on every frame, and a widget reports clicks as it is drawn (`if ui.button("Go").clicked()`); state lives in your own struct (`App`) | `App::handle_redraw()` |
| egui-winit, egui-wgpu | Translate winit events into egui input; draw egui's triangles with wgpu | `EguiRenderer` in `src/egui_tools.rs` |
| egui_commonmark, pulldown-cmark | Render Markdown with egui; parse Markdown | the page area in `handle_redraw()`, `get_heading()` |
| wgpu | Portable GPU API (Vulkan, Metal, ...). A *surface* is a window's drawable area; each frame takes a texture from its *swap chain* (`get_current_texture()`), draws, and `present()`s it | `AppState` in `src/app.rs` |
| softbuffer | Show a CPU-side pixel buffer (`u32` per pixel, `0x00RRGGBB`) in a window, no GPU code | QEMU's screen, `src/qemu_gfx.rs` |
| raw-window-handle | Common traits for native window handles, so wgpu, softbuffer and wasi-gfx can draw into a winit window | `MyWindowWrapper` in `src/winit_wasi.rs` |
| pollster | Runs an `async` function to completion on the current thread | `main()`, `App::resumed()` |
| reqwest (`blocking`) | HTTP client | `navigate()`, `download_wasm()` |

Sizes come as *physical* pixels (real screen pixels) or *logical* pixels
(physical divided by the display's scale factor, 2 on many high-DPI
screens): `PhysicalSize`, `LogicalSize` and friends in winit.

## WebAssembly

WebAssembly ("wasm") is a portable binary instruction format: a compiler
(clang, for QEMU) produces a `.wasm` file, and a runtime (wasmtime)
compiles it to the real CPU's machine code and runs it in a sandbox, where
it can only touch its own memory and the functions the host gives it.

**Module vs component.** A *core module* is the basic unit: functions, one
linear memory, tables, imports and exports. A *component* (the newer
component model) wraps core modules behind typed interfaces described in
WIT. QEMU is a core module; `breakout.wasm` and `triangle.wasm` (wasi-gfx
demos) are components. The first 8 bytes tell them apart, which is how
`App::window_event()` chooses:

| File kind | Header bytes |
|---|---|
| Core module (QEMU) | `00 61 73 6d 01 00 00 00` (`\0asm`, version 1) |
| Component | `00 61 73 6d 0d 00 01 00` |

**Linear memory.** A module's memory is one flat byte array, grown in
64 KiB pages; the code itself is kept apart from it. A "pointer" inside
wasm is just an offset into this array (32 bits in `wasm32`, hence at most
4 GiB, which must hold all of QEMU's data: its heap, its translation
buffer *and* the guest's RAM). When QEMU passes a pointer to a host
function, the host reads or writes that byte range of the memory, as
`with_guest()` and `with_guest_mut()` in `src/qemu_jit.rs` do.

**Imports and exports.** A module declares the functions (and possibly the
memory) it needs from outside, named by module and field, and the items it
offers. You can list them with `wasm-tools print --skeleton`:

| | The installed QEMU (multi-threaded build) |
|---|---|
| Imports | `env.memory` (shared); `env.host_gfx_*`, `env.host_net_*`, `env.host_main_wake`, `env.host_vcpu_run`; `env.wasi_jit_compile`, `wasi_jit_load_cached`, `wasi_jit_load_async`; `wasi_snapshot_preview1.*` (WASI); `wasi.thread-spawn` |
| Exports | `memory`, `__indirect_function_table`, `_initialize`, `wasi_qemu_init`, `wasi_qemu_step`, `wasi_qemu_quit`, `wasi_qemu_jit_reset`, `wasi_thread_start`, `wasi_vcpu_enter` |

All host imports are described in
[reference/host-imports.md](reference/host-imports.md).

**Function tables and call_indirect.** Wasm code cannot jump to an
address. A C function pointer becomes an index into a *table* of function
references, and calling through it is a `call_indirect`. QEMU exports its
table as `__indirect_function_table`. The JIT host (`jit_load()` in
`src/qemu_jit.rs`) grows that table by one entry holding a freshly
compiled translation block's entry function and returns the index, so
QEMU calls generated code like any function pointer; see
[07-jit-host.md](07-jit-host.md).

**Instances.** An *instance* is a module brought to life: its imports
bound, its memory and globals allocated. One module can have many
instances: every translation block module is instantiated, and the
multi-threaded QEMU has one instance of itself per thread, all sharing one
memory.

**Exceptions.** QEMU relies on C's `setjmp`/`longjmp` (a non-local jump
out of nested calls). clang compiles them to wasm exception handling, so
the engine must enable it (`Config::wasm_exceptions(true)`).

## wasmtime

| Type | What it is | In this code |
|---|---|---|
| `Config` | Settings for compiling and running (table below) | `run_wasm_module()`, `Wasm::new()` in `src/wasm.rs` |
| `Engine` | Compiler plus settings; shared, thread-safe | one per QEMU run, plus a second one that only precompiles QEMU optimised |
| Cranelift | wasmtime's compiler from wasm to machine code | `OptLevel::None` for translation blocks, `Speed` for QEMU's own code |
| `Module` | Compiled code of a core module (owned by the Engine, reusable). Can be precompiled to bytes (a `.cwasm` file) and loaded later | QEMU, cached as `qemu-<hash>.cwasm` in `$XDG_CACHE_HOME/m-browser/` or `~/.cache/m-browser/` by `load_qemu_module()`; each translation block |
| `Store<T>` | Holds all runtime objects (instances, memories, tables) plus your data `T`; used by one thread at a time. Nothing in it is freed until the Store is dropped | `Store<QemuHostState>`; replaced by `migrate_store()` and per vCPU in `run_vcpu()` |
| `QemuHostState` | The `T`: WASI context, JIT state, display state, cached memory and table, `StoreLimits` | `src/qemu_jit.rs` |
| `Linker<T>` | Resolves imports by name; `func_wrap()` turns a closure into a host function; `define()` adds an item such as a memory | `add_jit_to_linker()`, `add_gfx_to_linker()`, `add_net_to_linker()`, `add_threads_to_linker()` |
| `Caller<'_, T>` | Optional first argument of a host function: the calling Store's data (`data()`, `data_mut()`) and the caller's exports (`get_export()`) | most host functions |
| `Instance`, `InstancePre` | An instance; a module with imports already resolved, cheap to instantiate again | `Linker::instantiate()`; `QemuThreads::instantiate()` |
| `Memory`, `SharedMemory` | A Store's own memory (`data(&store)` is a `&[u8]`); a memory several threads and Stores share (its `data()` are `UnsafeCell<u8>`s, as anyone may write) | the `GuestMem` enum: `Plain` or `Shared` |
| `Table` | A function table; `grow()` returns the new entry's index | `jit_load()` |
| `Func`, `TypedFunc<P, R>` | A callable function; one with its signature checked once, faster to call | `get_typed_func::<(), i32>(&mut store, "wasi_qemu_step")` |
| `StoreLimits` | Per-Store caps; the default allows 10,000 instances | `qemu_store_limits()` raises it to 150,000 |

Configuration used for QEMU (`run_wasm_module()`):

| Setting | Why |
|---|---|
| `wasm_exceptions(true)` | QEMU's `setjmp`/`longjmp` |
| `cranelift_opt_level(OptLevel::None)` | Hundreds of thousands of tiny translation-block modules: compile time matters most. QEMU's own module is compiled once with `Speed` by a second engine and cached (`QEMU_WASM_OPT=speed` optimises everything, `none` nothing) |
| `shared_memory(true)` | The multi-threaded build imports a shared memory |
| `wasm_backtrace_details(Enable)` | Function names and source lines in error traces (turned off while profiling) |
| `epoch_interruption(true)` | Only with `QEMU_PROFILE` ([12-profiling.md](12-profiling.md)) |

The component runtime (`Wasm::new()`) uses `wasm_component_model(true)`,
`async_support(true)` and `epoch_interruption(true)`.

**Traps and exits.** A *trap* is wasm's crash (out-of-bounds access,
`unreachable`, a missed epoch deadline, ...). wasmtime returns it as an
`Err` from the call that was running, with a `WasmBacktrace` attached;
`wasi_jit_call` in `src/qemu_jit.rs` shows how to dig it out. When QEMU
calls `exit()`, wasmtime-wasi returns the error `I32Exit(status)`;
`run_qemu_reactor()` treats that as a normal end.

**Call hooks.** `Store::call_hook()` runs a closure on every transition
between wasm and host code (`CallHook::CallingHost`, `ReturningFromHost`,
...). It needs the `call-hook` Cargo feature. The profiler uses it to time
host calls; `install_store_hooks()` uses it to release the shared WASI
lock. A Store has one call hook: setting another replaces the first.

**Epochs.** With `epoch_interruption(true)`, compiled code checks a global
counter, the *epoch*, at every function entry and loop header.
`Engine::increment_epoch()` (from any thread) advances it; when a Store's
deadline (`set_epoch_deadline(n)`, n ticks from now) passes, wasmtime traps,
or calls the `epoch_deadline_callback()` if one is set. The profiler takes
its samples in that callback; the component runtime stops a component by
advancing the epoch, which makes it trap.

How `run_wasm_module()` puts this together (details in
[06-qemu-runner.md](06-qemu-runner.md)):

1. Build a `Config` and an `Engine`; load QEMU's `Module` (from the cache).
2. Look for an `env.memory` import: if present, this is the
   multi-threaded build.
3. Create a `Linker`, add WASI and the `env.*` host functions.
4. Build the WASI context: arguments, `WASI_*`/`QEMU_*` environment
   variables, preopened directories.
5. Create the `Store` with a `QemuHostState`, instantiate, and keep the
   exported memory and table in the state.
6. Call `_initialize`, `wasi_qemu_init`, then `wasi_qemu_step` in a loop.

## WASI preview 1

WASI is the system interface for wasm outside browsers: files, clocks,
random numbers, arguments, environment variables, exit. *Preview 1* is the
older, function-based version: plain functions imported from the module
`wasi_snapshot_preview1` (`fd_read`, `fd_pwrite`, `path_open`,
`clock_time_get`, `poll_oneoff`, `proc_exit`, ...), which is what QEMU's C
library (wasi-libc) calls. The `wasmtime-wasi` crate implements them:
`WasiCtxBuilder` collects the settings (`inherit_stdio()`, `args()`,
`env()`, `preopened_dir()`), `build_p1()` produces a `WasiP1Ctx`, and
`wasmtime_wasi::p1::add_to_linker_sync()` adds the functions to the Linker.

A WASI program sees no files except those under *preopened directories*:
host directories the host opens in advance and shows under a name of its
choosing. m-browser maps `./pc-bios` to `/bios` and the working directory
`.` to `/data`, so QEMU opens `/data/m.qcow2`. QEMU's stdout and stderr
are m-browser's (`inherit_stdio()`).

A WASI *command* exports `_start` (like `main`); a *reactor* exports
`_initialize` and functions the host calls when it wants. QEMU is built as
a reactor so that m-browser can run one main-loop iteration at a time.
Components use WASI *preview 2* instead (`wasmtime_wasi::p2`, `WasiCtx`
with a `ResourceTable`), see `Wasm::run_wasm()`.

## wasi-threads

Plain WASI has no threads. The *wasi-threads* convention, used by the
multi-threaded QEMU (built for the `wasm32-wasip1-threads` target), adds
them: the module imports a shared memory and a function
`wasi.thread-spawn(start_arg) -> tid`. When QEMU calls `pthread_create()`,
the host starts an OS thread, creates a new Store, instantiates the same
module again with the same shared memory, and calls its export
`wasi_thread_start(tid, start_arg)`. A negative return value means the
thread could not be created. In m-browser this is `QemuThreads::spawn()`;
the rest of the threading design is in [08-threads.md](08-threads.md).

## Where to learn more

- The Rust book: https://doc.rust-lang.org/book/ (chapters 4 on ownership,
  9 on errors, 16 on threads).
- The API documentation of every dependency, at the exact versions in
  Cargo.lock and without network access:

  ```sh
  cargo doc --offline --open    # from the m-browser directory
  ```

  Online, docs.rs has the same, for example
  https://docs.rs/wasmtime/41.0.4/wasmtime/ (pick the version from
  Cargo.lock).
- How the pieces fit in m-browser: [04-architecture.md](04-architecture.md).
