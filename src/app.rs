use crate::egui_tools::EguiRenderer;
use crate::wasm::Wasm;
use crate::winit_wasi::{MyWindowWrapper, WinitEventToSurfaceProxy};
use egui_wgpu::wgpu::SurfaceError;
use egui_wgpu::{wgpu, ScreenDescriptor};
use std::sync::{Arc, Mutex};
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

pub struct App {
    instance: wgpu::Instance,
    state: Option<AppState>,
    window: Option<Arc<Window>>,
    child_window: Option<Arc<Window>>,
    // wasi_surface: Option<wasi_surface_wasmtime::Surface>,
    wasi_event_handler: Option<WinitEventToSurfaceProxy>,
    parent_window_id: WindowId,
    child_window_id: WindowId,
    current_status: String,
    current_location: String,
    current_tab: String,
    current_page: String,
    tabs: Vec<Tab>,
    wasm_runtime: Arc<Mutex<Wasm>>,
    quit_pressed: bool,
    spawn_child_window: bool,
}

impl App {
    pub fn new() -> Self {
        let instance = egui_wgpu::wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        Self {
            instance,
            state: None,
            window: None,
            child_window: None,
            // wasi_surface: None,
            wasi_event_handler: None,
            parent_window_id: 1.into(),
            child_window_id: 2.into(),
            current_status: "Loading...".to_string(),
            current_location: "https://example.com/".to_string(),
            current_tab: "".to_string(),
            current_page: "".to_string(),
            tabs: vec![Tab {
                label: "".to_string(),
                location: "https://example.com/".to_string(),
                status: "Loaded".to_string(),
                contents: "".to_string(),
                back: Vec::new(),
                forward: Vec::new(),
            }],
            wasm_runtime: Arc::new(Mutex::new(Wasm::new().unwrap())),
            quit_pressed: false,
            spawn_child_window: false,
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
                                self.child_window_id = 2.into(); // hide child window
                                self.child_window = None;
                                for tab in &mut self.tabs {
                                    if tab.label == self.current_tab {
                                        tab.back.push(tab.location.clone());
                                        tab.forward.clear(); // clear forward history
                                        tab.location = self.current_location.clone();
                                        tab.contents = self.current_page.clone();
                                        tab.label = get_heading(tab.contents.clone());
                                        self.current_tab = tab.label.clone();
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
                                    self.child_window_id = 2.into(); // hide child window
                                    self.child_window = None;
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
                    for tab in &self.tabs {
                        if ui.button(&tab.label).clicked() {
                            self.current_location = tab.location.clone();
                            self.current_page = tab.contents.clone();
                            self.current_tab = tab.label.clone();
                        }
                    }

                    if ui.button("+").clicked() {
                        let new_tab = Tab {
                            label: "New Tab".to_owned(),
                            location: "https://example.com".to_owned(),
                            status: "Loaded".to_owned(),
                            contents: "".to_owned(),
                            back: Vec::new().to_owned(),
                            forward: Vec::new().to_owned(),
                        };
                        self.tabs.push(new_tab.clone());
                        self.current_tab = new_tab.label.clone();
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
                                        self.child_window_id = 2.into(); // hide child window
                                        self.child_window = None;
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
            .create_window(Window::default_attributes())
            .unwrap();
        pollster::block_on(self.set_window(window));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if self.quit_pressed {
            println!("Quit pressed, exiting.");
            event_loop.exit();
        }
        // let egui render to process the event first
        self.state
            .as_mut()
            .unwrap()
            .egui_renderer
            .handle_input(self.window.as_ref().unwrap(), &event);

        if let Some(wasi_event_handler) = &mut self.wasi_event_handler {
            wasi_event_handler.send_event(&event);
        }

        if self.spawn_child_window {
            self.spawn_child_window = false;
            println!("M key pressed");
            //let child_window = spawn_child_window(&Arc::try_unwrap(self.window.unwrap().unwrap(), event_loop);
            let child_window = Arc::new(spawn_child_window(self.window.as_ref().unwrap().as_ref(), event_loop));
            self.child_window = Some(Arc::clone(&child_window));
            // self.wasi_surface = Some(wasi_surface_wasmtime::Surface::new(Box::new(MyWindowWrapper(child_window))));

            let surface = wasi_surface_wasmtime::Surface::new(Box::new(MyWindowWrapper(child_window)));

            let surface_proxy: wasi_surface_wasmtime::SurfaceProxy = surface.proxy();
            self.wasi_event_handler = Some(WinitEventToSurfaceProxy::new(surface_proxy.clone()));

            let wasm_runtime = Arc::clone(&self.wasm_runtime);
            std::thread::spawn(move || {
                pollster::block_on(wasm_runtime.lock().unwrap().run_wasm("downloaded.wasm".to_string(), surface)).unwrap();
                // pollster::block_on(wasm_runtime.lock().unwrap().run_wasm("breakout.wasm".to_string(), surface)).unwrap();
            });

            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(16));
                    surface_proxy.animation_frame();
                }
            });

            let child_id = self.child_window.as_ref().unwrap().id();
            println!("Child window created with id: {child_id:?}");
            self.child_window_id = child_id;

        }

        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
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
        .with_inner_size(LogicalSize::new(400.0f32, 400.0f32))
        .with_position(Position::Logical(LogicalPosition::new(300.0, 300.0)))
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
    let resp = reqwest::blocking::get(location.clone())
        .and_then(|r| r.text())
        .map_err(|e| e.to_string());

    //println!("{:#?}", resp);
    //self.current_status = "Loaded".to_string();
    return resp.unwrap_or_else(|_| "Failed to load page".to_string());
}

pub fn download_wasm(url: String) -> String {
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

fn get_heading(contents: String) -> String {
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
    heading
}

fn is_wasm(filename: String) -> bool {
    if filename.ends_with(".wasm") {
        return true;
    }
    false
}
