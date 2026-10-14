use crate::egui_tools::EguiRenderer;
use crate::wasm::Wasm;
use crate::winit_wasi::{MyWindowWrapper, WinitEventToSurfaceProxy};
use egui_wgpu::wgpu::SurfaceError;
use egui_wgpu::{wgpu, ScreenDescriptor};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::fs;
use std::fs::File;
use std::io::{Read as _, SeekFrom, Seek};
use std::mem::{drop};
use std::sync::mpsc;
use system_interface::io::IoExt;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalSize, Position};
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};
use winit::raw_window_handle::HasRawWindowHandle;

use egui_commonmark::*;
use pulldown_cmark::{Parser, Options};

#[path = "fill.rs"]
mod fill;

#[derive(Clone)]
struct Tab {
    // Example stuff:
    label: String,
    location: String,
    status: String,
    identifier: i32,

    // currently loaded page of tab
    contents: String,

    // for history
    back: Vec<String>,
    forward: Vec<String>,
}

pub struct AppState {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface_config: wgpu::SurfaceConfiguration,
    pub surface: wgpu::Surface<'static>,
    pub scale_factor: f32,
    pub egui_renderer: EguiRenderer,
}

impl AppState {
    async fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        window: &Window,
        width: u32,
        height: u32,
    ) -> Self {
        let power_pref = wgpu::PowerPreference::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: power_pref,
                force_fallback_adapter: false,
                compatible_surface: Some(&surface),
            })
            .await
            .expect("Failed to find an appropriate adapter");

        let features = wgpu::Features::empty();
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: None,
                    required_features: features,
                    required_limits: Default::default(),
                    memory_hints: Default::default(),
                },
                None,
            )
            .await
            .expect("Failed to create device");

        let swapchain_capabilities = surface.get_capabilities(&adapter);
        let selected_format = wgpu::TextureFormat::Bgra8UnormSrgb;
        let swapchain_format = swapchain_capabilities
            .formats
            .iter()
            .find(|d| **d == selected_format)
            .expect("failed to select proper surface texture format!");

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: *swapchain_format,
            width,
            height,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 0,
            alpha_mode: swapchain_capabilities.alpha_modes[0],
            view_formats: vec![],
        };

        surface.configure(&device, &surface_config);

        let egui_renderer = EguiRenderer::new(&device, surface_config.format, None, 1, window);

        let scale_factor = 1.0;

        Self {
            device,
            queue,
            surface,
            surface_config,
            egui_renderer,
            scale_factor,
        }
    }

    fn resize_surface(&mut self, width: u32, height: u32) {
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);
    }
}

/// A QEMU core module running on its own thread.
struct QemuRun {
    /// Set to make QEMU quit; see `wasm::run_wasm_module`.
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// The disk image the browser's QEMU boots, relative to the working directory
/// (QEMU sees it as /data/m.qcow2).
const QEMU_DISK_IMAGE: &str = "m.qcow2";

/// Lock `path` for as long as the returned file stays open, so that no other
/// QEMU started by an m-browser writes the same disk image at the same time:
/// two writers corrupt a qcow2 image, and QEMU on WASI cannot lock files
/// itself. Fails if the image is locked already.
fn lock_disk_image(path: &str) -> Result<Option<File>, String> {
    let file = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        // Let QEMU report a missing or unreadable image.
        Err(_) => return Ok(None),
    };
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(fs::TryLockError::WouldBlock) => Err(format!(
            "{path} is in use by another QEMU; not starting a second one on the same disk image"
        )),
        Err(fs::TryLockError::Error(e)) => {
            eprintln!("Cannot lock {path} ({e}); starting QEMU anyway");
            Ok(None)
        }
    }
}

pub struct App {
    instance: wgpu::Instance,
    state: Option<AppState>,
    window: Option<Arc<Window>>,
    child_window: Option<Arc<Window>>,
    // wasi_surface: Option<wasi_surface_wasmtime::Surface>,
    wasi_event_handler: Option<WinitEventToSurfaceProxy>,
    /// Keyboard/pointer events destined for the QEMU wasm module.
    qemu_event_queue: Option<crate::qemu_gfx::GfxEventQueue>,
    /// The QEMU started by loading a core wasm module, while it runs.
    qemu: Option<QemuRun>,
    parent_window_id: WindowId,
    child_window_id: WindowId,
    current_status: String,
    current_location: String,
    current_tab: i32,
    current_page: String,
    tabs: Vec<Tab>,
    tab_counter: i32,
    wasm_runtime: Arc<Mutex<Wasm>>,
    quit_pressed: bool,
    spawn_child_window: bool,
    close_child_window: bool,
    sender: Option<mpsc::Sender<()>>,
    event_sender: Option<mpsc::Sender<()>>,
    event_receiver: Option<Arc<Mutex<mpsc::Receiver<()>>>>,

}

impl App {
    /// Ask the running QEMU, if any, to quit. Returns its thread, which ends
    /// once QEMU has closed its disk images.
    fn stop_qemu(&mut self) -> Option<std::thread::JoinHandle<()>> {
        let qemu = self.qemu.take()?;
        qemu.stop.store(true, Ordering::Relaxed);
        Some(qemu.thread)
    }

    /// Stop QEMU before the browser exits, giving it a few seconds to close
    /// its disk images so that they stay consistent.
    fn shutdown_qemu(&mut self) {
        if let Some(thread) = self.stop_qemu() {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    pub fn new() -> Self {
        let instance = egui_wgpu::wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let (tx, rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        Self {
            instance,
            state: None,
            window: None,
            child_window: None,
            // wasi_surface: None,
            wasi_event_handler: None,
            qemu_event_queue: None,
            qemu: None,
            parent_window_id: 1.into(),
            child_window_id: 2.into(),
            current_status: "Loading...".to_string(),
            current_location: "https://raw.githubusercontent.com/abemassry/m-browser/refs/heads/main/README.md".to_string(),
            current_tab: 0,
            current_page: "".to_string(),
            tabs: vec![Tab {
                label: "".to_string(),
                location: "https://raw.githubusercontent.com/abemassry/m-browser/refs/heads/main/README.md".to_string(),
                status: "Loaded".to_string(),
                contents: "".to_string(),
                back: Vec::new(),
                forward: Vec::new(),
                identifier: 0,
            }],
            tab_counter: 0,
            wasm_runtime: Arc::new(Mutex::new(Wasm::new(rx).unwrap())),
            quit_pressed: false,
            spawn_child_window: false,
            close_child_window: false,
            sender: Some(tx),
            event_sender: Some(event_tx),
            event_receiver: Some(Arc::new(Mutex::new(event_rx))),
        }
    }



    async fn set_window(&mut self, window: Window) {
        let window = Arc::new(window);
        let initial_width = 1920 * 2;
        let initial_height = 1080 * 2;

        let _ = window.request_inner_size(PhysicalSize::new(initial_width, initial_height));

        let surface = self
            .instance
            .create_surface(window.clone())
            .expect("Failed to create surface!");

        let state = AppState::new(
            &self.instance,
            surface,
            &window,
            initial_width,
            initial_width,
        )
        .await;

        self.window.get_or_insert(window);
        self.state.get_or_insert(state);
    }

    fn handle_resized(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.state.as_mut().unwrap().resize_surface(width, height);
        }
    }

    fn handle_redraw(&mut self) {
        // Attempt to handle minimizing window
        if let Some(window) = self.window.as_ref() {
            if let Some(min) = window.is_minimized() {
                if min {
                    println!("Window is minimized");
                    return;
                }
            }
        }

        let state = self.state.as_mut().unwrap();

        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [state.surface_config.width, state.surface_config.height],
            pixels_per_point: self.window.as_ref().unwrap().scale_factor() as f32
                * state.scale_factor,
        };

        let surface_texture = state.surface.get_current_texture();

        match surface_texture {
            Err(SurfaceError::Outdated) => {
                // Ignoring outdated to allow resizing and minimization
                println!("wgpu surface outdated");
                return;
            }
            Err(_) => {
                surface_texture.expect("Failed to acquire next swap chain texture");
                return;
            }
            Ok(_) => {}
        };

        let surface_texture = surface_texture.unwrap();

        let surface_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        let window = self.window.as_ref().unwrap();

        {
            state.egui_renderer.begin_frame(window);

            //egui::Window::new("winit + egui + wgpu says hello!")
            //    .resizable(true)
            //    .vscroll(true)
            //    .default_open(false)
            //    .show(state.egui_renderer.context(), |ui| {
            //        ui.label("Label!");

            //        if ui.button("Button!").clicked() {
            //            println!("boom!")
            //        }

            //        ui.separator();
            //        ui.horizontal(|ui| {
            //            ui.label(format!(
            //                "Pixels per point: {}",
            //                state.egui_renderer.context().pixels_per_point()
            //            ));
            //            if ui.button("-").clicked() {
            //                state.scale_factor = (state.scale_factor - 0.1).max(0.3);
            //            }
            //            if ui.button("+").clicked() {
            //                state.scale_factor = (state.scale_factor + 0.1).min(3.0);
            //            }
            //        });
            //    });
            //
            //    egui browser window
            egui::TopBottomPanel::top("top_panel")
                .show(state.egui_renderer.context(), |ui| {
                    egui::menu::bar(ui, |ui| {
                        ui.menu_button(egui_material_icons::icons::ICON_MENU, |ui| {
                            egui::widgets::global_theme_preference_buttons(ui);
                            if ui.button("Quit").clicked() {
                                println!("Quit button clicked");
                                self.quit_pressed = true;
                            }
                            if ui.button("spawn child window (M)").clicked() {
                                println!("Spawn child window button clicked");
                                self.spawn_child_window = true;
                            }
                        });
                        ui.add_space(3.0);
                        ui.add_space(1.0);
                        ui.button(egui_material_icons::icons::ICON_ARROW_BACK)
                            .on_hover_text("Back")
                            .clicked().then(|| {
                                if let Err(e) = back() {
                                    self.current_status = e.to_string();
                                } else {
                                    for tab in &mut self.tabs {
                                        if tab.identifier == self.current_tab {
                                            if tab.back.is_empty() {
                                                return;
                                            }
                                            println!("inside if statement for back button of tab");
                                            tab.forward.push(tab.location.clone());
                                            tab.location = tab.back.pop().unwrap_or_else(|| tab.location.clone());
                                            self.current_location = tab.location.clone();
                                            self.current_page = go(tab.location.clone());
                                            if is_wasm(self.current_location.clone()) {
                                                println!("opening child window, back button");
                                                self.spawn_child_window = true;
                                            } else {
                                                println!("Closing child window if open, back button");
                                                self.close_child_window = true;
                                            }
                                            tab.contents = self.current_page.clone();
                                            tab.label = get_heading(tab.location.clone(), tab.contents.clone());
                                            if let Some(window) = self.window.as_ref() {
                                                let title = format!("M - {}", tab.label);
                                                window.set_title(title.as_str());
                                            }
                                        }
                                    }
                                    self.current_status = "Loaded".to_string();
                                }
                            });
                        ui.add_space(1.0);
                        ui.button(egui_material_icons::icons::ICON_ARROW_FORWARD)
                            .on_hover_text("Forward")
                            .clicked().then(|| {
                                if let Err(e) = forward() {
                                    self.current_status = e.to_string();
                                } else {
                                    for tab in &mut self.tabs {
                                        if tab.identifier == self.current_tab {
                                            if tab.forward.is_empty() {
                                                return;
                                            }
                                            println!("inside if statement for forward button of tab");
                                            tab.back.push(tab.location.clone());
                                            tab.location = tab.forward.pop().unwrap_or_else(|| tab.location.clone());
                                            self.current_location = tab.location.clone();
                                            self.current_page = go(tab.location.clone());
                                            if is_wasm(self.current_location.clone()) {
                                                println!("opening child window, forward button");
                                                self.spawn_child_window = true;
                                            } else {
                                                println!("Closing child window if open, forward button");
                                                self.close_child_window = true;
                                            }
                                            tab.contents = self.current_page.clone();
                                            tab.label = get_heading(tab.location.clone(), tab.contents.clone());
                                            if let Some(window) = self.window.as_ref() {
                                                let title = format!("M - {}", tab.label);
                                                window.set_title(title.as_str());
                                            }
                                        }
                                    }
                                    self.current_status = "Loaded".to_string();
                                }
                            });
                        ui.add_space(1.0);
                        let button_width = 25.0;
                        let text_edit_width = ui.available_width() - button_width;
                        let response = ui.add_sized([text_edit_width.max(0.0), 20.0], egui::TextEdit::singleline(&mut self.current_location));
                        if response.lost_focus() && response.ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                            self.current_status = "Loading...".to_string();
                            self.current_page = go(self.current_location.clone());
                            if is_wasm(self.current_location.clone()) {
                                self.spawn_child_window = true;
                            } else {
                                println!("Closing child window if open, tab");
                                self.close_child_window = true;
                                for tab in &mut self.tabs {
                                    if tab.identifier == self.current_tab {
                                        tab.back.push(tab.location.clone());
                                        tab.forward.clear(); // clear forward history
                                        tab.location = self.current_location.clone();
                                        tab.contents = self.current_page.clone();
                                        tab.label = get_heading(tab.location.clone(), tab.contents.clone());
                                        if let Some(window) = self.window.as_ref() {
                                            let title = format!("M - {}", tab.label);
                                            window.set_title(title.as_str());
                                        }
                                        break;
                                    }
                                }

                                self.current_status = "Loaded".to_string();
                            }
                        }
                        ui.add_space(1.0);

                        ui.button(egui_material_icons::icons::ICON_KEYBOARD_DOUBLE_ARROW_RIGHT)
                            .on_hover_text("Go")
                            .clicked()
                            .then(|| {
                                self.current_status = "Loading...".to_string();
                                self.current_page = go(self.current_location.clone());
                                if is_wasm(self.current_location.clone()) {
                                    self.spawn_child_window = true;
                                } else {
                                    println!("Closing child window if open, tab");
                                    self.close_child_window = true;
                                    for tab in &mut self.tabs {
                                        if tab.identifier == self.current_tab {
                                            tab.back.push(tab.location.clone());
                                            tab.forward.clear(); // clear forward history
                                            tab.location = self.current_location.clone();
                                            tab.contents = self.current_page.clone();
                                            tab.label = get_heading(tab.location.clone(), tab.contents.clone());
                                            if let Some(window) = self.window.as_ref() {
                                                let title = format!("M - {}", tab.label);
                                                window.set_title(title.as_str());
                                            }
                                            break;
                                        }
                                    }

                                    self.current_status = "Loaded".to_string();
                                }
                            });

                    });
                });

                egui::TopBottomPanel::bottom("bottom_panel").show(state.egui_renderer.context(), |ui| {
                    let mut status_display: String = "Status: ".to_owned();
                    let status: &str = self.current_status.as_str();
                    status_display.push_str(status);
                    ui.label(status_display);
                });

                egui::SidePanel::left("side_panel").show(state.egui_renderer.context(), |ui| {
                    ui.separator();
                    for tab in &mut self.tabs {
                        if ui.button(&tab.label).clicked() {
                            self.current_tab = tab.identifier;
                            self.current_location = tab.location.clone();
                            self.current_page = go(self.current_location.clone());
                            if is_wasm(self.current_location.clone()) {
                                self.spawn_child_window = true;
                            } else {
                                println!("Closing child window if open, tab");
                                self.close_child_window = true;
                            }
                            println!("inside if statement for tab");
                            tab.back.push(tab.location.clone());
                            tab.forward.clear(); // clear forward history
                            tab.location = self.current_location.clone();
                            tab.contents = self.current_page.clone();
                            tab.label = get_heading(tab.location.clone(), tab.contents.clone());
                            if let Some(window) = self.window.as_ref() {
                                let title = format!("M - {}", tab.label);
                                window.set_title(title.as_str());
                            }

                            self.current_status = "Loaded".to_string();
                        }
                    }

                    if ui.button("+").clicked() {
                        self.tab_counter += 1;
                        let new_tab = Tab {
                            label: "New Tab".to_owned(),
                            location: "https://raw.githubusercontent.com/abemassry/m-browser/refs/heads/main/README.md".to_owned(),
                            status: "Loaded".to_owned(),
                            contents: "".to_owned(),
                            back: Vec::new().to_owned(),
                            forward: Vec::new().to_owned(),
                            identifier: self.tab_counter,
                        };
                        self.tabs.push(new_tab.clone());
                        self.current_tab = self.tab_counter;
                        self.current_location = new_tab.location.clone();
                        self.current_page = new_tab.contents.clone();

                    }
                });

                if self.child_window_id != 2.into() {
                    //println!("Child window is open");
                } else {
                    egui::CentralPanel::default().show(state.egui_renderer.context(), |ui| {

                        let binding = self.current_page.clone();
                        let markdown = binding.as_str();
                        let mut all_links: Vec<String> = Vec::new();

                        let mut cache = CommonMarkCache::default();

                        ui.style_mut().url_in_tooltip = true;
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            let parser = pulldown_cmark::Parser::new(markdown);
                            for event in parser {

                                match event {
                                    pulldown_cmark::Event::Start(contents) => {
                                        match contents {
                                            pulldown_cmark::Tag::Link{link_type: _, dest_url: url, title: _, id: _} => {
                                                cache.add_link_hook(url.to_string());
                                                all_links.push(url.to_string());
                                            },
                                            _ => {
                                            }
                                        }
                                    },
                                    _ => {
                                    }
                                }
                            }
                            ui.style_mut().url_in_tooltip = true;
                            CommonMarkViewer::new().show(ui, &mut cache, markdown);
                            for link in all_links {
                                if cache.get_link_hook(&link) == Some(true) {
                                    println!("Link was clicked {link}");
                                    self.current_location = link.clone();
                                    self.current_status = "Loading...".to_string();
                                    self.current_page = go(self.current_location.clone());
                                    if is_wasm(self.current_location.clone()) {
                                        self.spawn_child_window = true;
                                    } else {
                                        println!("Closing child window if open, link");
                                        self.close_child_window = true;
                                        self.current_status = "Loaded".to_string();
                                    }
                                    for tab in &mut self.tabs {
                                        if tab.identifier == self.current_tab {
                                            println!("inside if statement for tab");
                                            tab.back.push(tab.location.clone());
                                            tab.forward.clear(); // clear forward history
                                            tab.location = self.current_location.clone();
                                            tab.contents = self.current_page.clone();
                                            tab.label = get_heading(tab.location.clone(), tab.contents.clone());
                                            if let Some(window) = self.window.as_ref() {
                                                let title = format!("M - {}", tab.label);
                                                window.set_title(title.as_str());
                                            }
                                        }

                                        self.current_status = "Loaded".to_string();
                                    }
                                }
                                //ui.hyperlink_to(link, link);
                            }


                        });
                    });
                }
                // end of egui browser window



            state.egui_renderer.end_frame_and_draw(
                &state.device,
                &state.queue,
                &mut encoder,
                window,
                &surface_view,
                screen_descriptor,
            );
        }

        state.queue.submit(Some(encoder.finish()));
        surface_texture.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = event_loop
            .create_window(Window::default_attributes().with_title("M"))
            .unwrap();
        pollster::block_on(self.set_window(window));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        let close_child_window = self.close_child_window;
        if self.quit_pressed {
            println!("Quit pressed, exiting.");
            self.shutdown_qemu();
            event_loop.exit();
        }
        // let egui render to process the event first
        self.state
            .as_mut()
            .unwrap()
            .egui_renderer
            .handle_input(self.window.as_ref().unwrap(), &event);

        // Once QEMU has exited (e.g. it could not open its disk image), its
        // surface is gone: stop sending it events.
        if self.qemu.as_ref().is_some_and(|q| q.thread.is_finished()) {
            self.qemu = None;
            self.qemu_event_queue = None;
            self.wasi_event_handler = None;
        }

        if let Some(wasi_event_handler) = &mut self.wasi_event_handler {
            // Sending panics once the wasm module has exited and dropped its
            // surface: stop forwarding then, rather than take the browser down.
            let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                wasi_event_handler.send_event(&event)
            }));
            if sent.is_err() {
                self.wasi_event_handler = None;
            }
        }

        // Forward keyboard input to the QEMU wasm module.
        //
        // Routing deliberately does not key off the window id: the QEMU child
        // window never takes keyboard focus, so winit reports every key
        // against the parent window. Instead, send keys to QEMU whenever it is
        // running and egui does not want them itself (i.e. no text field such
        // as the URL bar is focused), so the browser chrome keeps working.
        let egui_wants_keys = self
            .state
            .as_ref()
            .map(|s| s.egui_renderer.context().wants_keyboard_input())
            .unwrap_or(false);
        let qemu_takes_keys = self.child_window.is_some() && !egui_wants_keys;

        // Forward the mouse to QEMU while the pointer is over its window.
        // Positions are in window pixels, i.e. in the frame as displayed;
        // the guest side maps them to its absolute pointing device
        // (QEMU's usb-tablet). Button codes are QEMU's InputButton values.
        if window_id == self.child_window_id {
            if let Some(queue) = &self.qemu_event_queue {
                let mut events: Vec<(i32, i32, i32)> = Vec::new();
                match &event {
                    WindowEvent::CursorMoved { position, .. } => {
                        events.push((1, 0, position.x as i32));
                        events.push((1, 1, position.y as i32));
                    }
                    WindowEvent::MouseInput { state, button, .. } => {
                        let code = match button {
                            winit::event::MouseButton::Left => Some(0),
                            winit::event::MouseButton::Middle => Some(1),
                            winit::event::MouseButton::Right => Some(2),
                            winit::event::MouseButton::Back => Some(5),
                            winit::event::MouseButton::Forward => Some(6),
                            winit::event::MouseButton::Other(_) => None,
                        };
                        if let Some(code) = code {
                            let down = matches!(state, ElementState::Pressed);
                            events.push((2, code, down as i32));
                        }
                    }
                    WindowEvent::MouseWheel { delta, .. } => {
                        let dy = match delta {
                            winit::event::MouseScrollDelta::LineDelta(_, y) => *y,
                            winit::event::MouseScrollDelta::PixelDelta(p) => p.y as f32,
                        };
                        // A wheel step is a press and release of wheel-up (3)
                        // or wheel-down (4).
                        if dy != 0.0 {
                            let code = if dy > 0.0 { 3 } else { 4 };
                            events.push((2, code, 1));
                            events.push((2, code, 0));
                        }
                    }
                    _ => {}
                }
                if !events.is_empty() {
                    if let Ok(mut q) = queue.lock() {
                        for (event_type, code, value) in events {
                            q.push(crate::qemu_gfx::InputEvent {
                                event_type,
                                code,
                                value,
                            });
                        }
                    }
                }
            }
        }


        if qemu_takes_keys {
            if let (Some(queue), WindowEvent::KeyboardInput { event: key, .. }) =
                (&self.qemu_event_queue, &event)
            {
                let down = matches!(key.state, ElementState::Pressed);

                if let PhysicalKey::Code(code) = key.physical_key {
                    match crate::qemu_keymap::keycode_to_evdev(code) {
                        Some(evdev) => {
                            if let Ok(mut q) = queue.lock() {
                                q.push(crate::qemu_gfx::InputEvent {
                                    event_type: 3, // key, as an evdev code
                                    code: evdev,
                                    value: if down { 1 } else { 0 },
                                });
                            }
                        }
                        None => {
                            eprintln!("[qemu-gfx] no evdev mapping for {code:?}, ignoring");
                        }
                    }

                    // Printable characters need a second, text-carrying event.
                    // QEMU's text consoles only translate a handful of
                    // navigation keys from key codes (see qcode_to_keysym in
                    // ui/console.c); everything else arrives as text. This
                    // mirrors what ui/gtk.c does for its VTE consoles.
                    //
                    // Keys that the key-code path already handles are skipped
                    // here so they are not delivered twice.
                    let handled_as_qcode = matches!(
                        code,
                        KeyCode::Tab
                            | KeyCode::Backspace
                            | KeyCode::Delete
                            | KeyCode::ArrowUp
                            | KeyCode::ArrowDown
                            | KeyCode::ArrowLeft
                            | KeyCode::ArrowRight
                            | KeyCode::Home
                            | KeyCode::End
                            | KeyCode::PageUp
                            | KeyCode::PageDown
                    );

                    if down && !handled_as_qcode {
                        if let Some(text) = key.logical_key.to_text() {
                            if let Ok(mut q) = queue.lock() {
                                for ch in text.chars() {
                                    q.push(crate::qemu_gfx::InputEvent {
                                        event_type: 5, // text, as a codepoint
                                        code: ch as i32,
                                        value: 0,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        if self.spawn_child_window {
            self.spawn_child_window = false;
            if self.child_window.is_some() {
                println!("About to stop wasm using wasm_runtime_events from new spawn");
                //pollster::block_on(wasm_runtime_events.lock().expect("REASON").stop_wasm()).unwrap();
                self.event_sender.as_ref().unwrap().send(()).unwrap();
                self.sender.as_ref().unwrap().send(()).unwrap();
                println!("Closing child window.");
                self.child_window_id = 2.into(); // hide child window
                self.child_window.clone().expect("REASON").set_visible(false);
                self.child_window = None;
                self.wasi_event_handler = None;
                let (tx, rx) = mpsc::channel();
                let (event_tx, event_rx) = mpsc::channel();
                self.sender = Some(tx);
                self.event_sender = Some(event_tx);
                self.event_receiver = Some(Arc::new(Mutex::new(event_rx)));
                self.wasm_runtime = Arc::new(Mutex::new(Wasm::new(rx).unwrap()));
            }
            let event_receiver = self.event_receiver.clone().unwrap();
            println!("Spawned child window.");

            if self.child_window.is_some() {
                println!("Child window already exists, closing it.");
                self.child_window_id = 2.into(); // hide child window
                self.child_window = None;
                self.wasi_event_handler = None;
            }
            println!("M key pressed");
            //let child_window = spawn_child_window(&Arc::try_unwrap(self.window.unwrap().unwrap(), event_loop);:
            let child_window = Arc::new(spawn_child_window(self.window.as_ref().unwrap().as_ref(), event_loop));
            self.child_window = Some(Arc::clone(&child_window));
            // self.wasi_surface = Some(wasi_surface_wasmtime::Surface::new(Box::new(MyWindowWrapper(child_window))));

            let surface = wasi_surface_wasmtime::Surface::new(Box::new(MyWindowWrapper(child_window)));

            let surface_proxy: wasi_surface_wasmtime::SurfaceProxy = surface.proxy();
            self.wasi_event_handler = Some(WinitEventToSurfaceProxy::new(surface_proxy.clone()));

            let wasm_runtime_start = Arc::clone(&self.wasm_runtime);

            // Resolve the actual wasm file path.
            // For file:// URLs, use the original file directly (download_wasm
            // may have failed to copy it properly, especially for large binaries).
            // For remote URLs, use the downloaded copy.
            let wasm_path = if self.current_location.starts_with("file://") {
                self.current_location.replace("file://", "")
            } else {
                "downloaded.wasm".to_string()
            };
            eprintln!("[detect] wasm_path={wasm_path}, exists={}", std::path::Path::new(&wasm_path).exists());

            // Detect whether the wasm file is a core module or a component.
            // Core modules: magic \0asm + version 0x01000000
            // Components:   magic \0asm + version 0x0d000100
            let is_core_module = {
                let mut header = [0u8; 8];
                match File::open(&wasm_path).and_then(|mut f| f.read_exact(&mut header)) {
                    Ok(()) => {
                        // Core module version bytes: [01, 00, 00, 00]
                        let is_core = header[4] == 0x01 && header[5] == 0x00 && header[6] == 0x00 && header[7] == 0x00;
                        eprintln!("[detect] wasm header bytes: {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} {:02x} => is_core={is_core}",
                            header[0], header[1], header[2], header[3],
                            header[4], header[5], header[6], header[7]);
                        is_core
                    }
                    Err(e) => {
                        eprintln!("[detect] Failed to read wasm header from '{}': {e}", wasm_path);
                        false // fall through to component path which will report the error
                    }
                }
            };

            if is_core_module {
                // Core module path — used for QEMU wasm binary
                // Pass the surface for graphical display via host_gfx_* functions
                println!("Detected core wasm module, using WASIp1 + JIT + GFX path");

                let gfx_event_queue = crate::qemu_gfx::new_event_queue();
                self.qemu_event_queue = Some(gfx_event_queue.clone());

                // Loading QEMU again replaces the running one, which must
                // first close the disk image the new one is about to open.
                let previous_qemu = self.stop_qemu();
                let stop = Arc::new(AtomicBool::new(false));
                let stop_qemu = stop.clone();

                let thread = std::thread::spawn(move || {
                    if let Some(previous) = previous_qemu {
                        println!("Waiting for the previous QEMU to quit");
                        let _ = previous.join();
                    }
                    let _image_lock = match lock_disk_image(QEMU_DISK_IMAGE) {
                        Ok(lock) => lock,
                        Err(message) => {
                            eprintln!("{message}");
                            return;
                        }
                    };
                let args: Vec<&str> = vec![
                    "qemu-system-x86_64",
                    "-L", "/bios",
                    "-drive", "file=/data/m.qcow2,format=qcow2",
                    // With QEMU's default of 128 MiB the guest's X desktop
                    // swaps constantly: everything crawls, and X stalls long
                    // enough for the guest kernel to drop keystrokes.
                    "-m", "512",
                    // 256 MiB of translated code. When the buffer fills up,
                    // QEMU discards all translations and has to translate
                    // (and the host recompile) its working set again; with
                    // 64 MiB that happened every few seconds while the
                    // desktop started, and it reached the desktop a minute
                    // later. Costs ~400 MiB more host memory.
                    "-accel", "tcg,tb-size=256",
                    // A USB tablet reports absolute positions, so the guest
                    // pointer follows the host pointer exactly (needed for
                    // X). XHCI rather than UHCI: no 1 kHz polling timer.
                    "-device", "qemu-xhci",
                    "-device", "usb-tablet",
                    // The window shows the guest's screen. These add QEMU's
                    // monitor (Ctrl+Alt+2) and the guest's serial port
                    // (Ctrl+Alt+3) as text consoles; Ctrl+Alt+1 switches back.
                    "-monitor", "vc",
                    "-serial", "vc",
                    // User-mode networking, provided by m-browser (see
                    // qemu_net.rs): DHCP, DNS, pings and UDP for now.
                    "-nic", "user",
                ];
                    // Use local paths within m-browser directory
                    let preopen_dirs: Vec<(&str, &str)> = vec![
                        ("pc-bios", "/bios"),
                        (".", "/data"),
                    ];
                    match crate::wasm::run_wasm_module(
                        &wasm_path,
                        &args,
                        &preopen_dirs,
                        Some(surface),
                        gfx_event_queue,
                        Some(stop_qemu),
                    ) {
                        Ok(_) => println!("QEMU core module ran successfully"),
                        Err(e) => eprintln!("Error running QEMU core module: {e:?}"),
                    }
                });
                self.qemu = Some(QemuRun { stop, thread });

                // Start animation frame / surface proxy thread for the core module too
                // so that the surface receives redraw ticks.
                std::thread::spawn(move || {
                    loop {
                        if event_receiver.lock().expect("REASON").try_recv().is_ok() {
                            println!("Close child window signal received in thread.");
                            break;
                        }
                        // Catch panics from animation_frame() — the surface channel
                        // may close if the QEMU thread exits or crashes.
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            surface_proxy.animation_frame();
                        }))
                        .is_err()
                        {
                            eprintln!("[qemu-gfx] animation_frame channel closed, stopping proxy thread");
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(16));
                    }
                });
            } else {
                // Component path — existing behavior for wasi-gfx components
                std::thread::spawn(move || {
                    match wasm_runtime_start.lock() {
                        Ok(mut guard) => {
                            match pollster::block_on(guard.run_wasm(wasm_path, surface)) {
                                Ok(_) => println!("Wasm ran successfully"),
                                Err(e) => println!("Error running wasm: {e}"),
                            }
                        },
                        Err(e) => {
                            println!("Failed to lock wasm runtime mutex: {e}");
                        }
                    }
                });

                std::thread::spawn(move || {
                    loop {
                        if event_receiver.lock().expect("REASON").try_recv().is_ok() {
                            println!("Close child window signal received in thread.");
                            break;
                        }
                        surface_proxy.animation_frame();
                        std::thread::sleep(std::time::Duration::from_millis(16));
                    }
                });
            }

            let child_id = self.child_window.as_ref().unwrap().id();
            println!("Child window created with id: {child_id:?}");
            self.child_window_id = child_id;

        }

        if self.close_child_window {
            self.close_child_window = false;
            //let wasm_runtime_events = Arc::clone(&self.wasm_runtime);
            if self.child_window.is_some() {
                println!("About to stop wasm using wasm_runtime_events");
                //pollster::block_on(wasm_runtime_events.lock().expect("REASON").stop_wasm()).unwrap();
                self.event_sender.as_ref().unwrap().send(()).unwrap();
                self.sender.as_ref().unwrap().send(()).unwrap();
                // QEMU finishes quitting on its own thread.
                drop(self.stop_qemu());
                println!("Closing child window.");
                self.child_window_id = 2.into(); // hide child window
                self.child_window.clone().expect("REASON").set_visible(false);
                self.child_window = None;
                self.wasi_event_handler = None;
                let (tx, rx) = mpsc::channel();
                let (event_tx, event_rx) = mpsc::channel();
                self.sender = Some(tx);
                self.event_sender = Some(event_tx);
                self.event_receiver = Some(Arc::new(Mutex::new(event_rx)));
                self.wasm_runtime = Arc::new(Mutex::new(Wasm::new(rx).unwrap()));
            }
        }

        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
                self.shutdown_qemu();
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                self.handle_redraw();

                self.window.as_ref().unwrap().request_redraw();
                if self.child_window_id != 2.into() {
                    fill::fill_window(self.child_window.as_ref().unwrap());
                    self.child_window.as_ref().unwrap().request_redraw();
                }
                //self.child_window.as_ref().unwrap().request_redraw();

            }
            WindowEvent::Resized(new_size) => {
                self.handle_resized(new_size.width, new_size.height);
            }
            _ => (),
        }
    }
}

fn spawn_child_window(parent: &Window, event_loop: &ActiveEventLoop) -> Window {
    let parent = parent.raw_window_handle().unwrap();
    let mut window_attributes = Window::default_attributes()
        .with_title("child window")
        .with_inner_size(LogicalSize::new(1080.0f32, 720.0f32))
        .with_position(Position::Logical(LogicalPosition::new(200.0, 27.0)))
        .with_visible(true);
    // `with_parent_window` is unsafe. Parent window must be a valid window.
    window_attributes = unsafe { window_attributes.with_parent_window(Some(parent)) };

    event_loop.create_window(window_attributes).unwrap()
}

pub fn back() -> Result<(), String> {
    // Implement back navigation logic here
    // For now, just return Ok
    println!("Back button pressed");
    Ok(())
}
pub fn forward() -> Result<(), String> {
    // Implement back navigation logic here
    // For now, just return Ok
    println!("Forward button pressed");
    Ok(())
}
pub fn go(url: String) -> String {
    // Implement go logic here
    println!("Going to URL: {}", url);
    println!("Go button pressed");
    let resp = navigate(url);
    return resp;
}

pub fn navigate(location: String) -> String {
    // Implement navigation logic here
    // For now, just return Ok
    println!("Navigating to URL: {}", location);
    if is_wasm(location.clone()) {
        println!("Downloading wasm file");
        let wasm_file = download_wasm(location.clone());
        return wasm_file;
    }
    if is_local_file(location.clone()) {
        let mut f = File::open(location.replace("file://", "")).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = String::new();
        f.read_to_string(&mut contents).unwrap();
        return contents;
    }
    let resp = reqwest::blocking::get(location.clone())
        .and_then(|r| r.text())
        .map_err(|e| e.to_string());

    //println!("{:#?}", resp);
    //self.current_status = "Loaded".to_string();
    return resp.unwrap_or_else(|_| "Failed to load page".to_string());
}

pub fn download_wasm(url: String) -> String {
    if is_local_file(url.clone()) {
        let wasm_path = url.replace("file://", "");
        println!("url is local file with url: {}", url);
        println!("Using local wasm file at {}", wasm_path);
        // Skip copy if source and destination resolve to the same file
        let dest = "downloaded.wasm";
        let src_canonical = std::fs::canonicalize(&wasm_path).ok();
        let dst_canonical = std::fs::canonicalize(dest).ok();
        if src_canonical.is_some() && src_canonical == dst_canonical {
            println!("Source and destination are the same file, skipping copy");
        } else {
            fs::copy(&wasm_path, dest).unwrap();
        }
        return wasm_path;
    }
    let resp = reqwest::blocking::get(url.clone())
        .and_then(|r| r.bytes())
        .map_err(|e| e.to_string());
    let wasm_bytes = resp.unwrap();
    // save wasm_bytes to a file
    // let wasm_path = format!("./{}.wasm", url.replace("https://", "").replace("/", "_"));
    let wasm_path = "downloaded.wasm".to_string();
    std::fs::write(&wasm_path, wasm_bytes).unwrap_or_else(|_| {
        eprintln!("Failed to write wasm file to {}", wasm_path);
    });
    return wasm_path;
}

fn get_heading(location: String, contents: String) -> String {
    let mut heading = String::new();
    let mut in_heading = false;
    let parser = Parser::new_ext(&contents, Options::empty());
    for event in parser {
        match event {
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::Heading { .. }) => {
                in_heading = true;
            }
            pulldown_cmark::Event::End(pulldown_cmark::TagEnd::Heading(_)) => {
                if in_heading {
                    break;
                }
            }
            pulldown_cmark::Event::Text(text) => {
                if in_heading {
                    heading.push_str(&text);
                }
            }
            _ => {}
        }
    }
    if heading.is_empty() {
        let end_part = location.split('/').last().unwrap_or("New Tab");
        heading = end_part.to_string();
    }

    heading
}

fn is_wasm(filename: String) -> bool {
    if filename.ends_with(".wasm") {
        return true;
    }
    false
}

fn is_local_file(path: String) -> bool {
    if path.starts_with("file://") {
        return true;
    }
    false
}
