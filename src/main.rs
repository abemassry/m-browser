mod app;
mod egui_tools;
mod headless;
mod qemu_gfx;
mod qemu_jit;
mod qemu_keymap;
mod qemu_net;
mod qemu_profile;
mod qemu_save;
mod qemu_threads;
mod saved_state;
mod tab_program;
mod wasm;
mod wasm_headless;
mod winit_wasi;

use winit::event_loop::{ControlFlow, EventLoop};

fn main() {
    // Development mode: run a QEMU wasm module without the GUI.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--qemu-headless") {
        if let Err(e) = headless::run(&args[2..]) {
            eprintln!("[headless] error: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // Any other core module, without a window: pause, hibernate, resume.
    if args.get(1).map(String::as_str) == Some("--wasm-headless") {
        match wasm_headless::run(&args[2..]) {
            Ok(status) => std::process::exit(status),
            Err(e) => {
                eprintln!("[wasm] error: {e:#}");
                std::process::exit(1);
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        pollster::block_on(run());
    }
}

async fn run() {
    let event_loop = EventLoop::new().unwrap();

    event_loop.set_control_flow(ControlFlow::Poll);

    let mut app = app::App::new();

    event_loop.run_app(&mut app).expect("Failed to run app");
}
