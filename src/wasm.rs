use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::error::Error as StdError;

use anyhow::Context;
// use clap::Parser;
use futures::executor::block_on;
// use wasi_frame_buffer_wasmtime::WasiFrameBufferView;
use wasi_graphics_context_wasmtime::WasiGraphicsContextView;
use wasi_surface_wasmtime::{Surface, SurfaceDesc, WasiSurfaceView};
use wasi_webgpu_wasmtime::{MainThreadSpawner, WasiWebGpuView};
use wasi_webgpu_wasmtime::reexports::{wgpu_core, wgpu_types};

use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Store,
};

use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi_io::IoView;

use winit::window::Window;

use crate::winit_wasi::MyWindowWrapper;
use crate::qemu_jit;
use crate::qemu_gfx;

// #[derive(clap::Parser, Debug)]
// struct RuntimeArgs {
//     /// The example name
//     #[arg(long)]
//     example: String,
// }

// wasmtime::component::bindgen!({
//     path: "../../wit/",
//     world: "example",
//     async: {
//         only_imports: [],
//     },
//     with: {
//         "wasi:graphics-context/graphics-context": wasi_graphics_context_wasmtime::wasi::graphics_context::graphics_context,
//         "wasi:surface/surface": wasi_surface_wasmtime::wasi::surface::surface,
//         // "wasi:frame-buffer/frame-buffer": wasi_frame_buffer_wasmtime::wasi::frame_buffer::frame_buffer,
//         "wasi:webgpu/webgpu": wasi_webgpu_wasmtime::wasi::webgpu::webgpu,
//     },
// });

struct HostState {
    pub table: ResourceTable,
    pub ctx: WasiCtx,
    pub wgpu_instance: Arc<wgpu_core::global::Global>,
    // pub surface_proxy: Option<wasi_surface_wasmtime::SurfaceProxy>,
    // pub main_thread_proxy: wasi_surface_wasmtime::WasiWinitEventLoopProxy,
    pub surface: Arc<Mutex<Option<Surface>>>,
}

impl HostState {
    // fn new(main_thread_proxy: wasi_surface_wasmtime::WasiWinitEventLoopProxy) -> Self {
    fn new() -> Self {
        Self {
            table: ResourceTable::new(),
            ctx: WasiCtxBuilder::new().inherit_stdio().build(),
            wgpu_instance: Arc::new(wgpu_core::global::Global::new(
                "webgpu",
                &wgpu_types::InstanceDescriptor {
                    backends: wgpu_types::Backends::all(),
                    flags: wgpu_types::InstanceFlags::from_build_config(),
                    backend_options: Default::default(),
                    memory_budget_thresholds: Default::default(),
                },
            )),
            // surface_proxy: None,
            surface: Arc::new(Mutex::new(None)),
        }
    }
}

impl IoView for HostState {
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl wasmtime::component::HasData for HostState {
    type Data<'a> = &'a mut HostState;
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}



impl WasiGraphicsContextView for HostState {}
// impl WasiFrameBufferView for HostState {}

struct UiThreadSpawner;

impl wasi_webgpu_wasmtime::MainThreadSpawner for UiThreadSpawner {
    async fn spawn<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // self.0.spawn(f).await
        println!("spawning");
        f()
    }
}

impl WasiWebGpuView for HostState {
    fn instance(&self) -> Arc<wgpu_core::global::Global> {
        Arc::clone(&self.wgpu_instance)
    }

    fn ui_thread_spawner(&self) -> Box<impl MainThreadSpawner> {
        println!("ui_thread_spawner");
        // todo!()
        Box::new(UiThreadSpawner)
    }
}

impl WasiSurfaceView for HostState {
    fn create_canvas(&self, desc: SurfaceDesc) -> Surface {
        println!("create_canvas");
        // block_on(self.main_thread_proxy.create_window(desc))
        // todo!()
        self.surface.lock().unwrap().take().unwrap()
        // wasi_surface_wasmtime::Surface::new(Box::new(MyWindowWrapper(surface)))
    }
}

// impl ExampleImports for HostState {
//     fn print(&mut self, s: String) {
//         println!("{s}");
//     }
// }


pub struct Wasm {
    engine: Engine,
    linker: Linker<HostState>,
    store: Store<HostState>,
    surface: Arc<Mutex<Option<Surface>>>,
}
impl Wasm {
    pub fn new(rx: mpsc::Receiver<()>) -> anyhow::Result<Wasm> {
        // env_logger::builder()
        //     .filter_level(log::LevelFilter::Info)
        //     .init();

        // let args = RuntimeArgs::parse();

        let mut config = Config::default();
        config.wasm_component_model(true);
        config.async_support(true);
        config.epoch_interruption(true);
        let engine = Engine::new(&config)?;
        let mut linker: Linker<HostState> = Linker::new(&engine);

        wasi_webgpu_wasmtime::add_to_linker(&mut linker)?;
        // wasi_frame_buffer_wasmtime::add_to_linker(&mut linker)?;
        wasi_graphics_context_wasmtime::add_to_linker(&mut linker)?;
        wasi_surface_wasmtime::add_only_surface_to_linker(&mut linker)?;
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

        // fn type_annotate<F>(val: F) -> F
        // where
        //     F: Fn(&mut HostState) -> &mut dyn ExampleImports,
        // {
        //     val
        // }
        // let closure = type_annotate::<_>(|t| t);
        // Example::add_to_linker_imports_get_host(&mut linker, closure)?;

        // let (main_thread_loop, main_thread_proxy) =
        //     wasi_surface_wasmtime::create_wasi_winit_event_loop();
        // wasi_surface_wasmtime:
        let host_state = HostState::new();
        let surface = Arc::clone(&host_state.surface);

        let mut store = Store::new(&engine, host_state);
        store.set_epoch_deadline(1);

        // main_thread_loop.run();
        let e2 = engine.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if rx.try_recv().is_ok() {
                    println!("Exiting epoch thread");
                    e2.increment_epoch();
                    break;
                }
            }
        });

        Ok(Wasm {
            engine,
            linker,
            store,
            surface,
        })
    }

    pub async fn run_wasm(&mut self, wasm_path: String, surface: Surface) -> anyhow::Result<()> {

        //self.surface.lock().unwrap().replace(surface);
        match self.surface.lock() {
            Ok(mut guard) => guard.replace(surface),
            Err(_) => {
                println!("Failed to lock surface mutex:");
                return Err(anyhow::anyhow!("Failed to lock surface mutex"));
            }
        };
        // let wasm_path = format!("./triangle.wasm");

        let component =
            Component::from_file(&self.engine, &wasm_path).context("Component file not found")?;

        // let instance = Example::instantiate_async(&mut self.store, &component, &self.linker)
        //     .await
        //     .unwrap();


        let instance =
            wasmtime_wasi::p2::bindings::Command::instantiate_async(&mut self.store, &component, &self.linker)
                .await
                .unwrap();


        // tokio::spawn(async move {
        match instance.wasi_cli_run().call_run(&mut self.store).await.context("failed to invoke `run` function") {
            Ok(Ok(())) => println!("Wasm run completed successfully"),
            Ok(Err(trap)) => println!("Wasm run trapped: {:?}", trap),
            Err(e) => println!("Error invoking wasm run: {}", e),
        };
            //instance.wasi_cli_run().call_run(&mut self.store)
            //    .await
            //    .context("failed to invoke `run` function")
            //    .unwrap()
            //    .unwrap();

        // });

        Ok(())
    }

    pub async fn stop_wasm(&mut self) -> anyhow::Result<()> {
        println!("Stopping wasm");
        // self.store.data_mut().surface_proxy = None;
        //self.store.data_mut().surface.lock().unwrap().take();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
//  Core module execution path for QEMU (WASIp1 + JIT)
// ---------------------------------------------------------------------------

/// Run a core WebAssembly module (not a Component) with WASIp1 support and
/// the QEMU JIT host extension.
///
/// This is used for the QEMU wasm binary which is a plain core module that
/// imports `wasi_snapshot_preview1` functions (WASIp1) plus three custom
/// `env::wasi_jit_*` functions.
///
/// `wasm_path`  — path to the .wasm file (e.g. "qemu-system-x86_64")
/// `qemu_args`  — command-line arguments to pass to the guest
///                (e.g. ["-drive", "file=/disk.qcow2,format=qcow2", "-m", "512"])
/// `preopen_dirs` — list of (host_path, guest_path) pairs to pre-open
///                  (e.g. [("/home/user/vm", "/")] so the guest can access disk images)
/// `surface`    — optional wasi_surface for display output (None = headless / -nographic)
/// `event_queue` — shared event queue for injecting input events from the main thread
pub fn run_wasm_module(
    wasm_path: &str,
    qemu_args: &[&str],
    preopen_dirs: &[(&str, &str)],
    surface: Option<wasi_surface_wasmtime::Surface>,
    event_queue: qemu_gfx::GfxEventQueue,
) -> anyhow::Result<()> {
    use qemu_jit::{QemuHostState, QemuJitState};
    use std::time::Instant;

    let t0 = Instant::now();

    // Engine — sync (no async_support), no component model needed
    // Enable standard wasm exception handling needed by QEMU's setjmp/longjmp
    // emulation (compiled with -mllvm -wasm-enable-sjlj -mllvm --wasm-use-legacy-eh=false
    // which emits try_table instructions using the standard exceptions proposal).
    let mut config = Config::default();
    config.wasm_exceptions(true);
    // Enable detailed backtraces with DWARF debug info (QEMU is compiled with -g)
    // This gives us function names and source locations in error traces.
    config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
    // Note: legacy_exceptions is NOT needed — QEMU binary uses standard
    // exception handling (try_table) compiled with --wasm-use-legacy-eh=false.
    //
    // Performance: TBs are compiled at runtime by Cranelift. The default opt
    // level (Speed) is very slow for our many small per-TB modules. Use
    // `None` to dramatically reduce per-TB compile latency. The QEMU module
    // itself only compiles once at startup so this trade-off is fine.
    config.cranelift_opt_level(wasmtime::OptLevel::None);
    let engine = Engine::new(&config)?;
    println!("[qemu] [{:>7.3}s] Engine created", t0.elapsed().as_secs_f64());

    // Core module linker
    let mut linker: wasmtime::Linker<QemuHostState> = wasmtime::Linker::new(&engine);

    // Add WASIp1 imports (wasi_snapshot_preview1::*)
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |state| &mut state.wasi)?;

    // Add QEMU JIT imports (env::wasi_jit_compile, env::wasi_jit_call, env::wasi_jit_drop)
    qemu_jit::add_jit_to_linker(&mut linker)?;

    // Add QEMU GFX imports (env::host_gfx_init, host_gfx_update_framebuffer, host_gfx_resize, host_gfx_poll_events)
    qemu_gfx::add_gfx_to_linker(&mut linker, event_queue)?;
    println!("[qemu] [{:>7.3}s] Linker configured", t0.elapsed().as_secs_f64());

    // Build WASI context
    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stdio();
    wasi_builder.args(qemu_args);

    // Forward QEMU's tuning/debug knobs (WASI_DEBUG, WASI_GFX_SCALE,
    // QEMU_DUMP_AT, ...) into the sandbox. Only these prefixes are passed so
    // the host's unrelated environment does not leak into the guest.
    for (key, value) in std::env::vars() {
        if key.starts_with("WASI_") || key.starts_with("QEMU_") {
            wasi_builder.env(&key, &value);
        }
    }

    // Pre-open directories so the guest can access disk images, etc.
    for (host_path, guest_path) in preopen_dirs {
        println!("[qemu] [{:>7.3}s] Preopening: {host_path} -> {guest_path}", t0.elapsed().as_secs_f64());
        wasi_builder.preopened_dir(
            host_path,
            guest_path,
            wasmtime_wasi::DirPerms::all(),
            wasmtime_wasi::FilePerms::all(),
        )?;
    }

    let wasi_ctx = wasi_builder.build_p1();

    // Build GFX state, setting the wasi_surface if we have one
    let mut gfx_state = qemu_gfx::QemuGfxState::new();
    if let Some(surf) = surface {
        gfx_state.wasi_surface = Some(Arc::new(surf));
    }

    let host_state = QemuHostState {
        wasi: wasi_ctx,
        jit: QemuJitState::new(&engine),
        gfx: gfx_state,
        guest_memory: None,
        pending_exception: false,
    };

    let mut store = Store::new(&engine, host_state);

    // Load the core module
    println!("[qemu] [{:>7.3}s] Loading module from: {wasm_path}", t0.elapsed().as_secs_f64());
    let module = match wasmtime::Module::from_file(&engine, wasm_path) {
        Ok(m) => {
            println!("[qemu] [{:>7.3}s] Module loaded successfully", t0.elapsed().as_secs_f64());
            m
        }
        Err(e) => {
            eprintln!("[qemu] [{:>7.3}s] Failed to load module: {e:?}", t0.elapsed().as_secs_f64());
            return Err(e).with_context(|| format!("failed to load wasm module: {wasm_path}"));
        }
    };

    // Instantiate
    println!("[qemu] [{:>7.3}s] Instantiating module...", t0.elapsed().as_secs_f64());
    let instance = linker
        .instantiate(&mut store, &module)
        .context("failed to instantiate QEMU module")?;
    println!("[qemu] [{:>7.3}s] Module instantiated", t0.elapsed().as_secs_f64());

    // Cache the guest memory reference in host state for JIT use
    if let Some(wasmtime::Extern::Memory(mem)) = instance.get_export(&mut store, "memory") {
        store.data_mut().guest_memory = Some(mem);
    }

    // Reactor builds (wasi_qemu_init/wasi_qemu_step exports) are driven one
    // main-loop iteration at a time; see run_qemu_reactor.
    if instance.get_func(&mut store, "wasi_qemu_step").is_some() {
        return run_qemu_reactor(store, instance, &linker, &module, t0);
    }

    // Get and call _start
    let start_func = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .context("failed to find _start export in QEMU module")?;

    println!("[qemu] [{:>7.3}s] Calling _start...", t0.elapsed().as_secs_f64());
    match start_func.call(&mut store, ()) {
        Ok(()) => {
            println!("[qemu] [{:>7.3}s] _start returned successfully", t0.elapsed().as_secs_f64());
        }
        Err(e) => {
            eprintln!("[qemu] [{:>7.3}s] _start failed!", t0.elapsed().as_secs_f64());
            eprintln!("[qemu] Error: {e:?}");
            // Print full causal chain
            let mut source = e.source();
            let mut depth = 1;
            while let Some(cause) = source {
                eprintln!("[qemu]   cause {depth}: {cause}");
                source = cause.source();
                depth += 1;
            }
            return Err(e).context("QEMU _start execution failed");
        }
    }
    Ok(())
}

/// Number of TB module instances to allow in one Store before moving QEMU
/// into a fresh one. Wasmtime frees instances only when their Store is
/// dropped; by default a Store may hold 10,000 instances, and every compiled
/// TB also holds executable mappings counted against vm.max_map_count
/// (usually 65,530). A single main-loop step can compile up to ~1,000 new
/// TBs, so this leaves headroom below both limits.
const MIGRATE_AFTER_INSTANCES: u32 = 7000;

/// Drive a QEMU build linked as a WASI reactor: `_initialize`, then
/// `wasi_qemu_init`, then `wasi_qemu_step` until it returns an exit status.
///
/// Because control returns here between main-loop iterations, no wasm frames
/// are live, which makes it safe to move QEMU into a new Store whenever the
/// per-TB JIT instances pile up (see `migrate_store`).
fn run_qemu_reactor(
    mut store: Store<qemu_jit::QemuHostState>,
    mut instance: wasmtime::Instance,
    linker: &wasmtime::Linker<qemu_jit::QemuHostState>,
    module: &wasmtime::Module,
    t0: std::time::Instant,
) -> anyhow::Result<()> {
    println!("[qemu] [{:>7.3}s] Initializing (reactor)...", t0.elapsed().as_secs_f64());
    instance
        .get_typed_func::<(), ()>(&mut store, "_initialize")
        .context("reactor module has no _initialize export")?
        .call(&mut store, ())
        .context("QEMU _initialize failed")?;
    let rc = instance
        .get_typed_func::<(), i32>(&mut store, "wasi_qemu_init")?
        .call(&mut store, ())
        .context("QEMU init failed")?;
    if rc != 0 {
        anyhow::bail!("wasi_qemu_init returned {rc}");
    }
    println!("[qemu] [{:>7.3}s] Running", t0.elapsed().as_secs_f64());

    let mut step = instance.get_typed_func::<(), i32>(&mut store, "wasi_qemu_step")?;
    loop {
        let status = match step.call(&mut store, ()) {
            Ok(status) => status,
            Err(e) => {
                // exit() inside QEMU surfaces as an I32Exit error.
                if let Some(exit) = e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                    println!("[qemu] exited with status {}", exit.0);
                    return Ok(());
                }
                eprintln!("[qemu] [{:>7.3}s] step failed: {e:?}", t0.elapsed().as_secs_f64());
                return Err(e).context("QEMU execution failed");
            }
        };
        if status >= 0 {
            println!("[qemu] exited with status {status}");
            return Ok(());
        }

        if store.data().jit.instances_in_store >= MIGRATE_AFTER_INSTANCES {
            let (new_store, new_instance) = migrate_store(store, instance, linker, module)?;
            store = new_store;
            instance = new_instance;
            step = instance.get_typed_func::<(), i32>(&mut store, "wasi_qemu_step")?;
        }
    }
}

/// Move a QEMU reactor instance into a fresh Store, freeing every per-TB JIT
/// module instance accumulated in the old one.
///
/// Must only be called between `wasi_qemu_step` calls, when no wasm frames
/// are live. QEMU's state lives in its linear memory (the only mutable
/// global, `__stack_pointer`, is back at its initial value between calls),
/// so copying memory into a new instance of the same module reproduces it.
/// Host-side state (WASI files, display, input queue) moves with the Store
/// data. QEMU is then told to forget its compiled-TB handles.
pub fn migrate_store(
    mut store: Store<qemu_jit::QemuHostState>,
    instance: wasmtime::Instance,
    linker: &wasmtime::Linker<qemu_jit::QemuHostState>,
    module: &wasmtime::Module,
) -> anyhow::Result<(Store<qemu_jit::QemuHostState>, wasmtime::Instance)> {
    let started = std::time::Instant::now();

    let memory = instance
        .get_memory(&mut store, "memory")
        .context("QEMU module has no memory export")?;
    let image: Vec<u8> = memory.data(&store).to_vec();
    let freed = store.data().jit.instances_in_store;

    // Dropping the old Store here is what actually frees the TB instances.
    let mut state = store.into_data();
    state.jit.reset_for_new_store();
    state.guest_memory = None;
    state.pending_exception = false;

    let mut store = Store::new(module.engine(), state);
    let instance = linker
        .instantiate(&mut store, module)
        .context("re-instantiating QEMU in a fresh store")?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .context("QEMU module has no memory export")?;
    let have = memory.data_size(&store);
    if image.len() > have {
        let pages = ((image.len() - have) as u64).div_ceil(65536);
        memory.grow(&mut store, pages).context("growing memory in the fresh store")?;
    }
    memory.data_mut(&mut store)[..image.len()].copy_from_slice(&image);
    store.data_mut().guest_memory = Some(memory);

    instance
        .get_typed_func::<(), ()>(&mut store, "wasi_qemu_jit_reset")?
        .call(&mut store, ())
        .context("wasi_qemu_jit_reset failed")?;

    eprintln!(
        "[qemu] moved QEMU to a fresh store: {} MiB copied, {} TB modules freed, {:.0} ms",
        image.len() >> 20,
        freed,
        started.elapsed().as_secs_f64() * 1e3
    );
    Ok((store, instance))
}
