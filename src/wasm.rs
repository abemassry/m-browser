use std::sync::{Arc, Mutex};

use anyhow::Context;
// use clap::Parser;
use futures::executor::block_on;
// use wasi_frame_buffer_wasmtime::WasiFrameBufferView;
use wasi_graphics_context_wasmtime::WasiGraphicsContextView;
use wasi_surface_wasmtime::{Surface, SurfaceDesc, WasiSurfaceView};
use wasi_webgpu_wasmtime::WasiWebGpuView;
use wasmtime::{
    component::{Component, Linker},
    Config, Engine, Store,
};

use wasmtime_wasi::{IoView, ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};
use winit::window::Window;

use crate::winit_wasi::MyWindowWrapper;

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
impl WasiView for HostState {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.ctx
    }
}



impl WasiGraphicsContextView for HostState {}
// impl WasiFrameBufferView for HostState {}

struct UiThreadSpawner;

impl wasi_webgpu_wasmtime::MainThreadSpawner for UiThreadSpawner {
    async fn spawn<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T + Send + Sync + 'static,
        T: Send + Sync + 'static,
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

    fn ui_thread_spawner(&self) -> Box<UiThreadSpawner> {
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
    pub fn new() -> anyhow::Result<Wasm> {
        // env_logger::builder()
        //     .filter_level(log::LevelFilter::Info)
        //     .init();

        // let args = RuntimeArgs::parse();

        let mut config = Config::default();
        config.wasm_component_model(true);
        config.async_support(true);
        let engine = Engine::new(&config)?;
        let mut linker: Linker<HostState> = Linker::new(&engine);

        wasi_webgpu_wasmtime::add_to_linker(&mut linker)?;
        // wasi_frame_buffer_wasmtime::add_to_linker(&mut linker)?;
        wasi_graphics_context_wasmtime::add_to_linker(&mut linker)?;
        wasi_surface_wasmtime::add_only_surface_to_linker(&mut linker)?;
        wasmtime_wasi::add_to_linker_sync(&mut linker)?;

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

        let store = Store::new(&engine, host_state);

        // main_thread_loop.run();

        Ok(Wasm {
            engine,
            linker,
            store,
            surface,
        })
    }

    pub async fn run_wasm(&mut self, wasm_path: String, surface: Surface) -> anyhow::Result<()> {

        self.surface.lock().unwrap().replace(surface);
        // let wasm_path = format!("./triangle.wasm");

        let component =
            Component::from_file(&self.engine, &wasm_path).context("Component file not found")?;

        // let instance = Example::instantiate_async(&mut self.store, &component, &self.linker)
        //     .await
        //     .unwrap();

        
        let instance =
            wasmtime_wasi::bindings::Command::instantiate_async(&mut self.store, &component, &self.linker)
                .await
                .unwrap();


        // tokio::spawn(async move {
            instance.wasi_cli_run().call_run(&mut self.store)
                .await
                .context("failed to invoke `run` function")
                .unwrap()
                .unwrap();

        // });

        Ok(())
    }
}
