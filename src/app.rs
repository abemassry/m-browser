use crate::egui_tools::EguiRenderer;
use crate::tab_program::{module_kind, Area, ModuleKind, QemuRole, QemuStart, TabProgram};
use crate::wasm_headless::{OutputPage, OUTPUT_LIMIT};
use egui_wgpu::wgpu::SurfaceError;
use egui_wgpu::{wgpu, ScreenDescriptor};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize, Position};
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::raw_window_handle::HasRawWindowHandle;
use winit::window::{Window, WindowId};

use egui_commonmark::*;
use pulldown_cmark::{Options, Parser};
use serde::{Deserialize, Serialize};

/// The address of a new tab (not loaded until you press Enter or Go).
const START_PAGE: &str = "https://raw.githubusercontent.com/abemassry/m-browser/refs/heads/main/README.md";

/// The address of the tab that boots a copy of `m.qcow2` to make the web
/// checkpoint from ("Set up web tabs").
const WEB_SETUP: &str = "about:web-setup";

/// The page a web address shows while there is no web checkpoint.
const NO_WEB_CHECKPOINT: &str = "# Web pages need the web checkpoint

m-browser shows web pages with Chromium, running in QEMU. Every web tab
starts from the same checkpoint: a moment saved once, when Chromium is up
and waiting for an address. There is none yet.

1. Open the menu and choose **Set up web tabs**. A new tab copies
   `m.qcow2` (about 2 GB, once per setup) and boots the copy. Chromium
   appears after a few minutes.
2. When Chromium waits for an address, choose **Save web checkpoint** in
   the menu. It takes a few seconds.
3. Come back to this tab and press Enter or Go again.
";

/// What an address shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageKind {
    /// A wasm module, run in the tab.
    Wasm,
    /// Markdown (or text): a `.md` address or a local file.
    Markdown,
    /// Anything else: Chromium in QEMU opens it.
    Web,
    /// `WEB_SETUP`.
    WebSetup,
    /// A saved state (`.wasmsavestate`), resumed in the tab.
    SavedState,
}

fn page_kind(address: &str) -> PageKind {
    if address == WEB_SETUP {
        return PageKind::WebSetup;
    }
    if is_wasm(address) {
        return PageKind::Wasm;
    }
    if address.ends_with(&format!(".{}", crate::saved_state::EXTENSION)) {
        return PageKind::SavedState;
    }
    let path = address.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    if address.is_empty() || is_local_file(address) || path.ends_with(".md") || path.ends_with(".markdown") {
        PageKind::Markdown
    } else {
        PageKind::Web
    }
}

/// A web tab's label: the host of its address.
fn web_label(address: &str) -> String {
    let rest = address.split_once("://").map_or(address, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    if host.is_empty() { address.to_string() } else { host.to_string() }
}

/// The browser's session, kept between runs in `SESSION_FILE` as JSON: its
/// tabs as addresses and history, the web checkpoint, and pointers to
/// saved states. At start only the focused tab is loaded; the others load
/// when they are opened, so a long session starts as fast as a short one.
#[derive(Serialize, Deserialize)]
struct SessionFile {
    version: u32,
    /// The id of the focused tab.
    current: String,
    /// The checkpoint every web tab starts from.
    web_checkpoint: Option<PathBuf>,
    tabs: Vec<SessionTab>,
}

#[derive(Serialize, Deserialize)]
struct SessionTab {
    id: String,
    label: String,
    location: String,
    #[serde(default)]
    back: Vec<String>,
    #[serde(default)]
    forward: Vec<String>,
    /// The Markdown page's scroll position, in points.
    #[serde(default)]
    scroll: f32,
    /// A hibernated program to resume instead of loading `location`
    /// afresh.
    #[serde(default)]
    saved_state: Option<PathBuf>,
}

/// The session file, in the working directory (the m-browser directory).
const SESSION_FILE: &str = "m-browser-session.json";
const SESSION_VERSION: u32 = 1;

/// How long quitting waits for programs to hibernate.
const SESSION_SAVE_TIMEOUT: Duration = Duration::from_secs(20);

fn read_session() -> Option<SessionFile> {
    let text = match fs::read_to_string(SESSION_FILE) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!("Cannot read {SESSION_FILE} ({e}); starting with a new session");
            return None;
        }
    };
    match serde_json::from_str::<SessionFile>(&text) {
        Ok(session) if session.version == SESSION_VERSION => Some(session),
        Ok(session) => {
            eprintln!("{SESSION_FILE} is version {}, not {SESSION_VERSION}; starting with a new session", session.version);
            None
        }
        Err(e) => {
            eprintln!("Cannot read {SESSION_FILE} ({e}); starting with a new session");
            None
        }
    }
}

/// Delete a web checkpoint that `new` replaced, and the disk images it was
/// made of (the setup's copy of `m.qcow2`, its overlays: `is_web_disk`),
/// unless `new` uses them too, as it does when one setup saved twice.
/// Web tabs still running on them go on: Linux deletes an open file's data
/// only when it is closed.
fn retire_web_checkpoint(old: &Path, new: &Path) {
    let keep: Vec<PathBuf> = crate::qemu_save::checkpoint_images(new)
        .unwrap_or_default()
        .iter()
        .filter_map(|p| fs::canonicalize(p).ok())
        .collect();
    let images = crate::qemu_save::checkpoint_images(old).unwrap_or_default();
    crate::tab_program::remove_file(old, "the web checkpoint a new one replaced");
    for image in images {
        let used = fs::canonicalize(&image).is_ok_and(|path| keep.contains(&path));
        if !used && crate::tab_program::is_web_disk(&image) {
            crate::tab_program::remove_file(&image, "a disk of the web checkpoint a new one replaced");
        }
    }
}

/// Delete a saved state that belonged to the tab `owner` alone (its name
/// ends with the tab's id), once nothing points to it any more. Saved
/// states of other origins, such as one opened as a page, are kept.
fn remove_own_state(path: &Path, owner: &str) {
    let suffix = format!("-{owner}.{}", crate::saved_state::EXTENSION);
    let ours = path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix));
    if !ours {
        return;
    }
    match fs::remove_file(path) {
        Ok(()) => println!("Removed {}, which nothing needs any more", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("Cannot remove {} ({e})", path.display()),
    }
}

/// The width of the tab list, in logical pixels: fixed, so that the page
/// area, which child windows cover, does not move with the tab labels.
const TAB_PANEL_WIDTH: f32 = 180.0;

/// Where a child window goes before the first frame has measured the page
/// area (never, in practice: actions come from a drawn UI).
const DEFAULT_AREA: Area = Area { x: 200, y: 27, width: 1080, height: 720 };

/// The size of a component's window, in logical pixels, or the page area's
/// if that is smaller. A component draws at the size its window has when
/// it starts, so its window keeps that size (`TabProgram::place`); the page
/// panel's background shows around it.
const COMPONENT_SIZE: (f32, f32) = (1080.0, 720.0);

/// A tab: its page, its history, and the program a `.wasm` page runs.
struct Tab {
    /// 12 hex digits (`saved_state::new_owner_id`): the names of the saved
    /// states and disk overlays of the tab's programs end with it.
    id: String,
    /// Tells tabs apart in the UI.
    identifier: i32,
    label: String,
    location: String,
    /// The page: Markdown text, or for a `.wasm` address the module's path.
    contents: String,
    /// What the status bar says while no program speaks for the tab.
    status: String,
    back: Vec<String>,
    forward: Vec<String>,
    /// The wasm module the page runs; it pauses while the tab is in the
    /// background (see tab_program.rs).
    program: Option<TabProgram>,
    /// A core module's output split into lines, and the version of the
    /// output they are of.
    output_lines: (Option<u64>, Vec<String>),
    /// Whether the page (and its program) is loaded: a tab restored from the
    /// session is not, until it is opened.
    loaded: bool,
    /// The saved state the tab's program resumes from, or resumed from: a
    /// hibernated program, from the session or opened as a page.
    saved_state: Option<PathBuf>,
    /// The Markdown page's scroll position, in points, as last drawn; and
    /// one to scroll to when the page is drawn next (from the session).
    scroll: f32,
    restore_scroll: Option<f32>,
}

impl Tab {
    fn new(identifier: i32) -> Tab {
        Tab {
            id: crate::saved_state::new_owner_id(),
            identifier,
            label: "New Tab".to_string(),
            location: START_PAGE.to_string(),
            contents: String::new(),
            status: "Press Enter or Go to load the page".to_string(),
            back: Vec::new(),
            forward: Vec::new(),
            program: None,
            output_lines: (None, Vec::new()),
            loaded: true,
            saved_state: None,
            scroll: 0.0,
            restore_scroll: None,
        }
    }

    /// A tab from the session file: only an address, its history and maybe
    /// a saved state until it is opened.
    fn restored(identifier: i32, saved: SessionTab) -> Tab {
        Tab {
            id: saved.id,
            label: saved.label,
            location: saved.location,
            status: "not loaded yet: open the tab".to_string(),
            back: saved.back,
            forward: saved.forward,
            loaded: false,
            saved_state: saved.saved_state,
            restore_scroll: (saved.scroll > 0.0).then_some(saved.scroll),
            ..Tab::new(identifier)
        }
    }

    fn status_text(&self) -> String {
        self.program.as_ref().map_or_else(|| self.status.clone(), TabProgram::status)
    }
}

/// What the UI asks for. Doing it may need winit's event loop (to create
/// a child window, to exit), which `window_event()` has and
/// `handle_redraw()` does not: the UI queues actions while it is drawn, and
/// `window_event()` carries them out right after.
enum Action {
    /// Load an address in the current tab, adding it to the tab's history.
    Go(String),
    Back,
    Forward,
    Reload,
    NewTab,
    SwitchTo(i32),
    Close(i32),
    /// Open the tab that makes the web checkpoint.
    SetUpWeb,
    /// Save the web checkpoint from the current (setup) tab.
    SaveWebCheckpoint,
    /// Load the current tab if it is only restored from the session.
    OpenCurrent,
    Quit,
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
    /// The address bar's text.
    current_location: String,
    current_tab: i32,
    tabs: Vec<Tab>,
    tab_counter: i32,
    /// Queued by the UI, done by `window_event()`.
    actions: Vec<Action>,
    /// The page area as last drawn, in physical pixels: where the current
    /// tab's child window goes.
    content: Option<Area>,
    /// Physical pixels per logical pixel, as last drawn.
    scale: f32,
    /// The checkpoint every web tab starts from.
    web_checkpoint: Option<PathBuf>,
    /// The tab whose next checkpoint becomes the web checkpoint.
    pending_web_checkpoint: Option<i32>,
    /// The threads of QEMUs that were asked to quit and may still be
    /// closing their disk images: m-browser waits for them before exiting.
    stopping: Vec<JoinHandle<()>>,
}

impl App {
    pub fn new() -> Self {
        let instance = egui_wgpu::wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let mut app = Self {
            instance,
            state: None,
            window: None,
            current_location: START_PAGE.to_string(),
            current_tab: 0,
            tabs: vec![Tab::new(0)],
            tab_counter: 0,
            actions: Vec::new(),
            content: None,
            scale: 1.0,
            web_checkpoint: None,
            pending_web_checkpoint: None,
            stopping: Vec::new(),
        };
        if let Some(session) = read_session() {
            app.restore(session);
        }
        app
    }

    /// Take the tabs of the last session: all as addresses, history and
    /// saved-state pointers; the focused one is loaded at the first event
    /// (`Action::OpenCurrent`), the others when they are opened.
    fn restore(&mut self, session: SessionFile) {
        let tabs: Vec<Tab> = session
            .tabs
            .into_iter()
            .enumerate()
            .map(|(i, saved)| Tab::restored(i as i32, saved))
            .collect();
        if tabs.is_empty() {
            return;
        }
        println!("Restoring {} tab(s) from {SESSION_FILE}", tabs.len());
        self.tab_counter = tabs.len() as i32 - 1;
        self.current_tab = tabs.iter().find(|t| t.id == session.current).map_or(0, |t| t.identifier);
        self.tabs = tabs;
        if let Some(i) = self.current_index() {
            self.current_location = self.tabs[i].location.clone();
        }
        self.web_checkpoint = session.web_checkpoint.filter(|path| {
            let exists = path.is_file();
            if !exists {
                eprintln!("The web checkpoint {} is gone; set up web tabs again", path.display());
            }
            exists
        });
        self.actions.push(Action::OpenCurrent);
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
        let App { state, window, current_location, current_tab, tabs, actions, content, scale, .. } = self;
        let (Some(state), Some(window)) = (state.as_mut(), window.as_ref()) else { return };

        // Attempt to handle minimizing window
        if let Some(true) = window.is_minimized() {
            println!("Window is minimized");
            return;
        }

        let screen_descriptor = ScreenDescriptor {
            size_in_pixels: [state.surface_config.width, state.surface_config.height],
            pixels_per_point: window.scale_factor() as f32 * state.scale_factor,
        };

        let surface_texture = match state.surface.get_current_texture() {
            Ok(texture) => texture,
            Err(SurfaceError::Outdated) => {
                // Ignoring outdated to allow resizing and minimization
                println!("wgpu surface outdated");
                return;
            }
            Err(SurfaceError::Lost) => {
                // The swap chain must be recreated.
                state.surface.configure(&state.device, &state.surface_config);
                return;
            }
            Err(e) => {
                // Timeout: the display is not taking frames, e.g. while the
                // screen is blanked or powered down. Any other failure: the
                // frame cannot be shown either. Skip it and try again on the
                // next redraw; panicking here killed the VM with the window.
                use std::sync::atomic::{AtomicU64, Ordering};
                static SKIPPED: AtomicU64 = AtomicU64::new(0);
                let n = SKIPPED.fetch_add(1, Ordering::Relaxed);
                if n.is_power_of_two() || n == 0 {
                    eprintln!("[gfx] no swap chain texture ({e}); skipping frame ({} so far)", n + 1);
                }
                return;
            }
        };

        let surface_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });

        state.egui_renderer.begin_frame(window);
        let ctx = state.egui_renderer.context().clone();
        let page = draw_ui(&ctx, current_location, *current_tab, tabs, actions);
        // egui works in points, which are winit's logical pixels
        // (pixels_per_point is the window's scale factor); child windows
        // are placed in whole physical pixels.
        let ppp = ctx.pixels_per_point();
        *scale = ppp;
        *content = Some(Area {
            x: (page.min.x * ppp).round() as i32,
            y: (page.min.y * ppp).round() as i32,
            width: (page.width() * ppp).round().max(1.0) as u32,
            height: (page.height() * ppp).round().max(1.0) as u32,
        });
        state.egui_renderer.end_frame_and_draw(
            &state.device,
            &state.queue,
            &mut encoder,
            window,
            &surface_view,
            screen_descriptor,
        );

        state.queue.submit(Some(encoder.finish()));
        surface_texture.present();
    }

    fn index_of(&self, identifier: i32) -> Option<usize> {
        self.tabs.iter().position(|t| t.identifier == identifier)
    }

    fn current_index(&self) -> Option<usize> {
        self.index_of(self.current_tab)
    }

    fn set_title(&self) {
        if let (Some(window), Some(i)) = (self.window.as_ref(), self.current_index()) {
            window.set_title(&format!("M - {}", self.tabs[i].label));
        }
    }

    /// Do what the UI asked for. Returns false once m-browser is exiting.
    fn act(&mut self, event_loop: &ActiveEventLoop, action: Action) -> bool {
        match action {
            Action::Go(address) => self.load(event_loop, address, true),
            Action::Back => {
                let Some(i) = self.current_index() else { return true };
                if let Some(previous) = self.tabs[i].back.pop() {
                    let here = self.tabs[i].location.clone();
                    self.tabs[i].forward.push(here);
                    self.load(event_loop, previous, false);
                }
            }
            Action::Forward => {
                let Some(i) = self.current_index() else { return true };
                if let Some(next) = self.tabs[i].forward.pop() {
                    let here = self.tabs[i].location.clone();
                    self.tabs[i].back.push(here);
                    self.load(event_loop, next, false);
                }
            }
            Action::Reload => {
                if let Some(i) = self.current_index() {
                    let here = self.tabs[i].location.clone();
                    self.load(event_loop, here, false);
                }
            }
            Action::NewTab => self.new_tab(),
            Action::SwitchTo(identifier) => self.switch_to(event_loop, identifier),
            Action::Close(identifier) => self.close(event_loop, identifier),
            Action::OpenCurrent if self.content.is_none() => {
                // Not before the first frame has measured the page area,
                // where a program's window goes.
                self.actions.push(Action::OpenCurrent);
            }
            Action::OpenCurrent => self.open_current(event_loop),
            Action::SetUpWeb => {
                self.new_tab();
                self.load(event_loop, WEB_SETUP.to_string(), false);
            }
            Action::SaveWebCheckpoint => {
                let Some(i) = self.current_index() else { return true };
                if let Some(program) = self.tabs[i].program.as_mut() {
                    if program.qemu_role() == Some(QemuRole::Setup) && program.save_checkpoint() {
                        self.pending_web_checkpoint = Some(self.current_tab);
                    }
                }
            }
            Action::Quit => {
                println!("Quit: stopping every tab's program");
                self.shutdown();
                event_loop.exit();
                return false;
            }
        }
        true
    }

    /// Show `address` in the current tab, in place of its page and program.
    /// A web tab given another web address keeps its QEMU: its Chromium
    /// goes there.
    fn load(&mut self, event_loop: &ActiveEventLoop, address: String, add_to_history: bool) {
        let address = address.trim().to_string();
        // A checkpoint written just now is taken before the tab's program
        // may be stopped.
        self.tick_programs();
        let Some(i) = self.current_index() else { return };
        let kind = page_kind(&address);
        // The page replaces whatever the tab resumed from.
        if let Some(old) = self.tabs[i].saved_state.take() {
            remove_own_state(&old, &self.tabs[i].id);
        }
        self.tabs[i].restore_scroll = None;
        self.tabs[i].loaded = true;
        let stays = kind == PageKind::Web && self.tabs[i].program.as_mut().is_some_and(|p| p.open_url(&address));
        if !stays {
            if let Some(program) = self.tabs[i].program.take() {
                self.retire(program);
            }
        }
        let parent = self.window.clone();
        let (area, scale) = (self.content.unwrap_or(DEFAULT_AREA), self.scale);
        let checkpoint = self.web_checkpoint.clone();
        let tab = &mut self.tabs[i];
        if add_to_history && tab.location != address && !tab.location.is_empty() {
            tab.back.push(tab.location.clone());
            tab.forward.clear();
        }
        tab.location = address.clone();
        tab.output_lines = (None, Vec::new());
        let no_window = || TabProgram::Failed("the browser has no window yet".into());
        match kind {
            PageKind::Wasm => {
                tab.label = get_heading(&address, "");
                match fetch_wasm(&address) {
                    Ok(path) => {
                        tab.contents = path.to_string_lossy().into_owned();
                        tab.status = format!("running {}", path.display());
                        tab.program = Some(match parent {
                            Some(parent) => start_program(event_loop, &parent, &path, &tab.id, area, scale),
                            None => no_window(),
                        });
                    }
                    Err(message) => {
                        tab.contents = format!("# Cannot open this page\n\n{message}\n");
                        tab.status = message;
                    }
                }
            }
            PageKind::Markdown => {
                tab.contents = navigate(&address);
                tab.label = get_heading(&address, &tab.contents);
                tab.status = "Loaded".to_string();
            }
            PageKind::Web => {
                tab.label = web_label(&address);
                if stays {
                    tab.status = format!("opening {address}");
                } else if let Some(checkpoint) = checkpoint {
                    tab.contents.clear();
                    tab.status = format!("opening {address}");
                    tab.program = Some(match parent {
                        Some(parent) => start_qemu_tab(
                            event_loop,
                            &parent,
                            QemuStart::Resume { checkpoint, url: Some(address.clone()) },
                            &tab.id,
                            area,
                        ),
                        None => no_window(),
                    });
                } else {
                    tab.contents = NO_WEB_CHECKPOINT.to_string();
                    tab.status = "there is no web checkpoint yet".to_string();
                }
            }
            PageKind::SavedState => {
                let path = PathBuf::from(address.trim_start_matches("file://"));
                tab.label = get_heading(&address, "");
                tab.contents = path.to_string_lossy().into_owned();
                tab.status = format!("resuming {}", path.display());
                tab.program = Some(start_saved(event_loop, parent.as_deref(), &path, &tab.id, area));
                tab.saved_state = Some(path);
            }
            PageKind::WebSetup => {
                tab.label = "Web setup".to_string();
                tab.contents.clear();
                tab.status = "setting up the web checkpoint".to_string();
                tab.program = Some(match (parent, fs::canonicalize("qemu-system-x86_64.wasm")) {
                    (Some(parent), Ok(module)) => {
                        start_qemu_tab(event_loop, &parent, QemuStart::Setup(module), &tab.id, area)
                    }
                    (None, _) => no_window(),
                    (_, Err(e)) => TabProgram::Failed(format!(
                        "cannot find qemu-system-x86_64.wasm in the working directory: {e}"
                    )),
                });
            }
        }
        self.current_location = address;
        self.set_title();
    }

    /// Act on what the tabs' programs reported, on every frame: a checkpoint
    /// from the web setup tab becomes the web checkpoint.
    fn tick_programs(&mut self) {
        let mut new_checkpoint = false;
        for tab in &mut self.tabs {
            let Some(program) = tab.program.as_mut() else { continue };
            for result in program.tick() {
                if self.pending_web_checkpoint != Some(tab.identifier) {
                    continue;
                }
                self.pending_web_checkpoint = None;
                match result {
                    Ok(path) => {
                        println!("The web checkpoint is {}", path.display());
                        if let Some(old) = self.web_checkpoint.replace(path.clone()) {
                            retire_web_checkpoint(&old, &path);
                        }
                        new_checkpoint = true;
                    }
                    Err(message) => eprintln!("The web checkpoint was not saved: {message}"),
                }
            }
        }
        // Keep it even if the browser does not quit cleanly.
        if new_checkpoint {
            self.write_session();
        }
    }

    /// Load the current tab if it is only restored from the session: resume
    /// its saved state, or load its address.
    fn open_current(&mut self, event_loop: &ActiveEventLoop) {
        let Some(i) = self.current_index() else { return };
        if self.tabs[i].loaded {
            return;
        }
        let location = self.tabs[i].location.clone();
        let scroll = self.tabs[i].restore_scroll;
        if let Some(state) = self.tabs[i].saved_state.clone() {
            let parent = self.window.clone();
            let area = self.content.unwrap_or(DEFAULT_AREA);
            match start_saved(event_loop, parent.as_deref(), &state, &self.tabs[i].id, area) {
                TabProgram::Failed(message) => {
                    eprintln!("Cannot resume {} ({message}); loading {location} instead", state.display());
                }
                program => {
                    let tab = &mut self.tabs[i];
                    tab.loaded = true;
                    tab.contents = state.to_string_lossy().into_owned();
                    tab.status = format!("resuming {}", state.display());
                    tab.program = Some(program);
                    self.current_location = location;
                    self.set_title();
                    return;
                }
            }
        }
        self.load(event_loop, location, false);
        self.tabs[i].restore_scroll = scroll;
    }

    /// Open a new tab and make it the current one; the tab it leaves pauses.
    fn new_tab(&mut self) {
        if let Some(program) = self.current_index().and_then(|i| self.tabs[i].program.as_mut()) {
            program.pause();
        }
        self.tab_counter += 1;
        let tab = Tab::new(self.tab_counter);
        self.current_tab = tab.identifier;
        self.current_location = tab.location.clone();
        self.tabs.push(tab);
        self.set_title();
    }

    /// Make another tab the current one: the one left pauses, the one
    /// shown goes on where it was. Nothing is loaded again.
    fn switch_to(&mut self, event_loop: &ActiveEventLoop, identifier: i32) {
        if identifier == self.current_tab {
            return;
        }
        let Some(new) = self.index_of(identifier) else { return };
        if let Some(program) = self.current_index().and_then(|i| self.tabs[i].program.as_mut()) {
            program.pause();
        }
        self.current_tab = identifier;
        self.current_location = self.tabs[new].location.clone();
        if !self.tabs[new].loaded {
            // Restored from the session: loaded only now.
            self.open_current(event_loop);
        } else if let Some(program) = self.tabs[new].program.as_mut() {
            program.resume();
        }
        self.set_title();
    }

    /// Close a tab and stop its program. Closing the current tab shows its
    /// neighbour; closing the last one leaves a new, empty tab.
    fn close(&mut self, event_loop: &ActiveEventLoop, identifier: i32) {
        // A checkpoint written just now is taken first.
        self.tick_programs();
        if self.pending_web_checkpoint == Some(identifier) {
            // Its QEMU quits, which ends the save.
            self.pending_web_checkpoint = None;
        }
        let Some(i) = self.index_of(identifier) else { return };
        let tab = self.tabs.remove(i);
        if let Some(program) = tab.program {
            self.retire(program);
        }
        if let Some(state) = &tab.saved_state {
            remove_own_state(state, &tab.id);
        }
        if self.tabs.is_empty() {
            self.tab_counter += 1;
            self.tabs.push(Tab::new(self.tab_counter));
        }
        if identifier == self.current_tab {
            let next = i.min(self.tabs.len() - 1);
            self.current_tab = self.tabs[next].identifier;
            self.current_location = self.tabs[next].location.clone();
            if !self.tabs[next].loaded {
                self.open_current(event_loop);
            } else if let Some(program) = self.tabs[next].program.as_mut() {
                program.resume();
            }
            self.set_title();
        }
    }

    /// Stop a program for good, keeping QEMU's thread to wait for at exit.
    /// A web tab's or a setup's overlays are deleted: a web tab comes back
    /// by its address, so nothing needs them, except the images the web
    /// checkpoint is made of.
    fn retire(&mut self, program: TabProgram) {
        self.stopping.retain(|thread| !thread.is_finished());
        let owner = program.overlay_owner();
        if let Some(thread) = program.stop() {
            self.stopping.push(thread);
        }
        if let Some(owner) = owner {
            let keep = self
                .web_checkpoint
                .as_deref()
                .and_then(|checkpoint| crate::qemu_save::checkpoint_images(checkpoint).ok())
                .unwrap_or_default();
            crate::tab_program::remove_overlays(&owner, &keep);
        }
    }

    /// Before the browser exits: save what can be saved for the next start
    /// (`save_programs`), stop every program, giving the QEMUs up to 5 s in
    /// all to close their disk images so that those stay consistent, and
    /// write the session file.
    fn shutdown(&mut self) {
        self.save_programs();
        let programs: Vec<TabProgram> = self.tabs.iter_mut().filter_map(|tab| tab.program.take()).collect();
        for program in programs {
            self.retire(program);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.stopping.iter().any(|thread| !thread.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.write_session();
    }

    /// Have the programs that can be saved do so (core modules with save
    /// points hibernate; see `TabProgram::save_for_session`), waiting up to
    /// `SESSION_SAVE_TIMEOUT`. A loaded tab's saved state becomes the new
    /// one, or none, and the old one is deleted.
    fn save_programs(&mut self) {
        let mut waiting: Vec<usize> = (0..self.tabs.len())
            .filter(|&i| self.tabs[i].program.as_mut().is_some_and(TabProgram::save_for_session))
            .collect();
        if !waiting.is_empty() {
            println!("Saving {} tab(s) for the next start", waiting.len());
        }
        let mut saved: Vec<(usize, PathBuf)> = Vec::new();
        let deadline = Instant::now() + SESSION_SAVE_TIMEOUT;
        while !waiting.is_empty() && Instant::now() < deadline {
            waiting.retain(|&i| {
                let Some(program) = self.tabs[i].program.as_mut() else { return false };
                for result in program.tick() {
                    match result {
                        Ok(path) => {
                            saved.push((i, path));
                            return false;
                        }
                        Err(message) => {
                            eprintln!("Cannot save {} ({message})", self.tabs[i].location);
                            return false;
                        }
                    }
                }
                program.is_running()
            });
            std::thread::sleep(Duration::from_millis(20));
        }
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if !tab.loaded {
                continue; // still the state it was restored with
            }
            let new = saved.iter().find(|(j, _)| *j == i).map(|(_, path)| path.clone());
            if let Some(old) = tab.saved_state.take() {
                if Some(&old) != new.as_ref() {
                    remove_own_state(&old, &tab.id);
                }
            }
            tab.saved_state = new;
        }
    }

    /// Write the session file: every tab but the web setup's, which is only
    /// for making the web checkpoint.
    fn write_session(&self) {
        let tabs: Vec<SessionTab> = self
            .tabs
            .iter()
            .filter(|t| t.location != WEB_SETUP)
            .map(|t| SessionTab {
                id: t.id.clone(),
                label: t.label.clone(),
                location: t.location.clone(),
                back: t.back.clone(),
                forward: t.forward.clone(),
                scroll: t.restore_scroll.unwrap_or(t.scroll),
                saved_state: t.saved_state.clone(),
            })
            .collect();
        let current = self
            .current_index()
            .map(|i| &self.tabs[i])
            .filter(|t| t.location != WEB_SETUP)
            .or_else(|| self.tabs.iter().find(|t| t.location != WEB_SETUP))
            .map(|t| t.id.clone())
            .unwrap_or_default();
        let session = SessionFile { version: SESSION_VERSION, current, web_checkpoint: self.web_checkpoint.clone(), tabs };
        let written = serde_json::to_string_pretty(&session)
            .map_err(|e| e.to_string())
            .and_then(|json| {
                let tmp = format!("{SESSION_FILE}.tmp");
                fs::write(&tmp, json + "\n").and_then(|_| fs::rename(&tmp, SESSION_FILE)).map_err(|e| e.to_string())
            });
        match written {
            Ok(()) => println!("Wrote {SESSION_FILE} ({} tab(s))", session.tabs.len()),
            Err(e) => eprintln!("Cannot write {SESSION_FILE}: {e}"),
        }
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
        let is_main = self.window.as_ref().is_some_and(|w| w.id() == window_id);

        // egui sees the main window's events only. A child window's pointer
        // events are at the child's coordinates: egui would take them for
        // events at the same place of the main window, where a click could
        // hit the address bar or a tab.
        if is_main {
            if let (Some(state), Some(window)) = (self.state.as_mut(), self.window.as_ref()) {
                state.egui_renderer.handle_input(window, &event);
            }
        } else if let (WindowEvent::MouseInput { state: ElementState::Pressed, .. }, Some(state)) =
            (&event, self.state.as_ref())
        {
            // A click into a program's window gives it the keyboard: a text
            // field of egui's (the address bar) lets go of the focus.
            let ctx = state.egui_renderer.context();
            if let Some(focused) = ctx.memory(|m| m.focused()) {
                ctx.memory_mut(|m| m.surrender_focus(focused));
            }
        }

        // Input for the current tab's program. Keys go to it while egui
        // does not want them itself (no text field such as the address bar
        // has the focus), so the browser chrome keeps working.
        let keys = !self
            .state
            .as_ref()
            .is_some_and(|s| s.egui_renderer.context().wants_keyboard_input());
        let current = self.current_tab;
        if let Some(program) = self.tabs.iter_mut().find(|t| t.identifier == current).and_then(|t| t.program.as_mut()) {
            program.handle_event(window_id, &event, keys);
        }

        match event {
            WindowEvent::CloseRequested if is_main => {
                println!("The close button was pressed; stopping");
                self.actions.push(Action::Quit);
            }
            WindowEvent::RedrawRequested if is_main => {
                self.handle_redraw();
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }
                self.tick_programs();
                // The current tab's window covers the page area, also after
                // the main window was resized.
                let (current, area) = (self.current_tab, self.content);
                if let (Some(program), Some(area)) =
                    (self.tabs.iter_mut().find(|t| t.identifier == current).and_then(|t| t.program.as_mut()), area)
                {
                    program.place(area);
                }
            }
            WindowEvent::Resized(new_size) if is_main => {
                self.handle_resized(new_size.width, new_size.height);
            }
            _ => (),
        }

        for action in std::mem::take(&mut self.actions) {
            if !self.act(event_loop, action) {
                return;
            }
        }
    }
}

/// Describe the whole UI for one frame (egui is an immediate-mode UI),
/// queueing what the user asks for in `actions`. Returns the page area, in
/// egui points.
fn draw_ui(
    ctx: &egui::Context,
    address: &mut String,
    current: i32,
    tabs: &mut [Tab],
    actions: &mut Vec<Action>,
) -> egui::Rect {
    use egui_material_icons::icons;

    egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
        egui::menu::bar(ui, |ui| {
            ui.menu_button(icons::ICON_MENU, |ui| {
                egui::widgets::global_theme_preference_buttons(ui);
                if ui.button("Reload").clicked() {
                    actions.push(Action::Reload);
                    ui.close_menu();
                }
                ui.separator();
                if ui
                    .button("Set up web tabs")
                    .on_hover_text("Boot a copy of m.qcow2, to save the web checkpoint from")
                    .clicked()
                {
                    actions.push(Action::SetUpWeb);
                    ui.close_menu();
                }
                let in_setup = tabs
                    .iter()
                    .find(|t| t.identifier == current)
                    .and_then(|t| t.program.as_ref())
                    .and_then(TabProgram::qemu_role)
                    == Some(QemuRole::Setup);
                if ui
                    .add_enabled(in_setup, egui::Button::new("Save web checkpoint"))
                    .on_hover_text("When Chromium waits for an address: every new web tab starts from this moment")
                    .on_disabled_hover_text("In the web setup tab (Set up web tabs)")
                    .clicked()
                {
                    actions.push(Action::SaveWebCheckpoint);
                    ui.close_menu();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    println!("Quit button clicked");
                    actions.push(Action::Quit);
                }
            });
            ui.add_space(4.0);
            if ui.button(icons::ICON_ARROW_BACK).on_hover_text("Back").clicked() {
                actions.push(Action::Back);
            }
            ui.add_space(1.0);
            if ui.button(icons::ICON_ARROW_FORWARD).on_hover_text("Forward").clicked() {
                actions.push(Action::Forward);
            }
            ui.add_space(1.0);
            let button_width = 25.0;
            let text_edit_width = ui.available_width() - button_width;
            let response = ui.add_sized([text_edit_width.max(0.0), 20.0], egui::TextEdit::singleline(address));
            if response.lost_focus() && response.ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                actions.push(Action::Go(address.clone()));
            }
            ui.add_space(1.0);
            if ui.button(icons::ICON_KEYBOARD_DOUBLE_ARROW_RIGHT).on_hover_text("Go").clicked() {
                actions.push(Action::Go(address.clone()));
            }
        });
    });

    let current_index = tabs.iter().position(|t| t.identifier == current);

    egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
        let status = current_index.map(|i| tabs[i].status_text()).unwrap_or_default();
        ui.label(format!("Status: {status}"));
    });

    egui::SidePanel::left("side_panel")
        .resizable(false)
        .exact_width(TAB_PANEL_WIDTH)
        .show(ctx, |ui| {
            ui.separator();
            for tab in tabs.iter() {
                ui.horizontal(|ui| {
                    if ui.small_button(icons::ICON_CLOSE).on_hover_text("Close tab").clicked() {
                        actions.push(Action::Close(tab.identifier));
                    }
                    if ui
                        .selectable_label(tab.identifier == current, short_label(&tab.label))
                        .on_hover_text(&tab.location)
                        .clicked()
                    {
                        actions.push(Action::SwitchTo(tab.identifier));
                    }
                });
            }
            if ui.button("+").on_hover_text("New tab").clicked() {
                actions.push(Action::NewTab);
            }
        });

    // What the panels above leave: the page area.
    let page = ctx.available_rect();

    egui::CentralPanel::default().show(ctx, |ui| {
        let Some(i) = current_index else { return };
        let tab = &mut tabs[i];
        enum View {
            Markdown,
            Output(OutputPage),
            Failed(String),
            Window,
        }
        let view = match &tab.program {
            None => View::Markdown,
            Some(TabProgram::Core(core)) => View::Output(core.output().clone()),
            Some(TabProgram::Failed(message)) => View::Failed(message.clone()),
            // QEMU and components draw into their child window, which
            // covers this panel once it is shown.
            Some(_) => View::Window,
        };
        match view {
            View::Markdown => show_markdown(ui, tab, actions),
            View::Output(page) => show_output(ui, tab, &page),
            View::Failed(message) => {
                ui.heading("This page cannot run");
                ui.label(message);
            }
            // Seen until QEMU's window is shown: waiting for the disk
            // image, or why QEMU could not start.
            View::Window => {
                ui.label(tab.status_text());
            }
        }
    });
    page
}

/// A Markdown page. Each page of each tab keeps its own scroll position:
/// egui remembers it per scroll-area id.
fn show_markdown(ui: &mut egui::Ui, tab: &mut Tab, actions: &mut Vec<Action>) {
    let mut cache = CommonMarkCache::default();
    ui.style_mut().url_in_tooltip = true;
    let mut area = egui::ScrollArea::vertical().id_salt(("page", tab.identifier, tab.location.as_str()));
    if let Some(offset) = tab.restore_scroll.take() {
        area = area.vertical_scroll_offset(offset);
    }
    let markdown = tab.contents.as_str();
    let output = area.show(ui, |ui| {
            // Register every link, so that a click navigates in m-browser
            // instead of opening the system browser.
            let mut links: Vec<String> = Vec::new();
            for event in Parser::new(markdown) {
                if let pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link { dest_url, .. }) = event {
                    cache.add_link_hook(dest_url.to_string());
                    links.push(dest_url.to_string());
                }
            }
            CommonMarkViewer::new().show(ui, &mut cache, markdown);
            for link in links {
                if cache.get_link_hook(&link) == Some(true) {
                    println!("Link was clicked {link}");
                    actions.push(Action::Go(link));
                }
            }
        });
    tab.scroll = output.state.offset.y;
}

/// A core module's output, following its end as it grows.
fn show_output(ui: &mut egui::Ui, tab: &mut Tab, page: &OutputPage) {
    let version = page.version();
    if tab.output_lines.0 != Some(version) {
        let (text, trimmed) = page.text();
        let mut lines = Vec::new();
        if trimmed {
            lines.push(format!("[earlier output dropped: only about the last {} KiB are kept]", OUTPUT_LIMIT >> 10));
        }
        lines.extend(text.lines().map(str::to_string));
        tab.output_lines = (Some(version), lines);
    }
    let lines = &tab.output_lines.1;
    if lines.is_empty() {
        ui.label("(no output yet)");
        return;
    }
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both()
        .id_salt(("output", tab.identifier))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show_rows(ui, row_height, lines.len(), |ui, rows| {
            for line in &lines[rows] {
                ui.add(egui::Label::new(egui::RichText::new(line).monospace()).extend());
            }
        });
}

/// A tab label short enough for the tab list.
fn short_label(label: &str) -> String {
    const MAX: usize = 18;
    if label.chars().count() <= MAX {
        label.to_string()
    } else {
        format!("{}...", label.chars().take(MAX - 3).collect::<String>())
    }
}

/// Start the module at `path` for the tab `owner`, in a new child window in
/// the page area `area` if it draws (`scale`: physical pixels per logical
/// pixel).
fn start_program(
    event_loop: &ActiveEventLoop,
    parent: &Window,
    path: &Path,
    owner: &str,
    area: Area,
    scale: f32,
) -> TabProgram {
    let kind = match module_kind(path) {
        Ok(kind) => kind,
        Err(e) => return TabProgram::Failed(format!("{e:#}")),
    };
    eprintln!("[tab {owner}] {}: {kind:?} module", path.display());
    let window = if kind.needs_window() {
        // QEMU's window covers the page area (QEMU scales its picture to
        // it) and appears once QEMU runs (`QemuTab::update_visibility`).
        // A component's keeps the size it starts with (COMPONENT_SIZE).
        let size = if kind == ModuleKind::Qemu {
            area
        } else {
            Area {
                width: area.width.min((COMPONENT_SIZE.0 * scale).round() as u32),
                height: area.height.min((COMPONENT_SIZE.1 * scale).round() as u32),
                ..area
            }
        };
        match spawn_child_window(parent, event_loop, size, kind != ModuleKind::Qemu) {
            Ok(window) => Some(Arc::new(window)),
            Err(e) => return TabProgram::Failed(format!("cannot create a window for it: {e}")),
        }
    } else {
        None
    };
    TabProgram::start(kind, path, owner, window)
}

/// Resume a saved state in a tab: a hibernated core module, or a QEMU
/// checkpoint (in a new, hidden child window over the page area).
fn start_saved(event_loop: &ActiveEventLoop, parent: Option<&Window>, path: &Path, owner: &str, area: Area) -> TabProgram {
    use crate::saved_state::{Engine, StateReader};
    let engine = match StateReader::open(path) {
        Ok(reader) => reader.header.engine,
        Err(e) => return TabProgram::Failed(format!("{e:#}")),
    };
    match (engine, parent) {
        (Engine::WasmSnapshot, _) => TabProgram::resume_core(path),
        (Engine::QemuMigration, Some(parent)) => start_qemu_tab(
            event_loop,
            parent,
            QemuStart::Resume { checkpoint: path.to_path_buf(), url: None },
            owner,
            area,
        ),
        (Engine::QemuMigration, None) => TabProgram::Failed("the browser has no window yet".into()),
    }
}

/// Start QEMU for a web tab or the web setup, in a new, hidden child window
/// covering the page area (it appears once QEMU runs).
fn start_qemu_tab(event_loop: &ActiveEventLoop, parent: &Window, start: QemuStart, owner: &str, area: Area) -> TabProgram {
    match spawn_child_window(parent, event_loop, area, false) {
        Ok(window) => TabProgram::start_qemu(start, owner, Arc::new(window)),
        Err(e) => TabProgram::Failed(format!("cannot create a window for QEMU: {e}")),
    }
}

/// A window inside the main window, covering `area` (physical pixels), for
/// a tab's program to draw into.
fn spawn_child_window(parent: &Window, event_loop: &ActiveEventLoop, area: Area, visible: bool) -> Result<Window, String> {
    let parent = parent.raw_window_handle().map_err(|e| e.to_string())?;
    let mut window_attributes = Window::default_attributes()
        .with_title("child window")
        .with_inner_size(PhysicalSize::new(area.width, area.height))
        .with_position(Position::Physical(PhysicalPosition::new(area.x, area.y)))
        .with_visible(visible);
    // `with_parent_window` is unsafe. Parent window must be a valid window.
    window_attributes = unsafe { window_attributes.with_parent_window(Some(parent)) };

    event_loop.create_window(window_attributes).map_err(|e| e.to_string())
}

/// The text of a Markdown address: a `file://` file, or an HTTP GET.
pub fn navigate(location: &str) -> String {
    println!("Navigating to URL: {location}");
    if is_local_file(location) {
        return fs::read_to_string(location.trim_start_matches("file://"))
            .unwrap_or_else(|e| format!("Failed to load page: {e}"));
    }
    reqwest::blocking::get(location)
        .and_then(|r| r.text())
        .unwrap_or_else(|_| "Failed to load page".to_string())
}

/// The module a `.wasm` address names, as an absolute path on this
/// computer. A `file://` module is used where it is. Anything else is
/// downloaded into the working directory as `downloaded-<hash>.wasm`, named
/// after its content: a tab's module then stays as it was while other tabs
/// download theirs (a saved state names its module and resumes only with
/// the very same one).
fn fetch_wasm(url: &str) -> Result<PathBuf, String> {
    use sha2::{Digest, Sha256};

    if is_local_file(url) {
        let path = url.trim_start_matches("file://");
        return fs::canonicalize(path).map_err(|e| format!("cannot open {path}: {e}"));
    }
    println!("Downloading wasm file {url}");
    let bytes = reqwest::blocking::get(url)
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.bytes())
        .map_err(|e| format!("cannot download {url}: {e}"))?;
    let hash = Sha256::digest(&bytes);
    let name = format!("downloaded-{}.wasm", hash[..6].iter().map(|b| format!("{b:02x}")).collect::<String>());
    if !Path::new(&name).exists() {
        let tmp = format!("{name}.tmp");
        fs::write(&tmp, &bytes)
            .and_then(|_| fs::rename(&tmp, &name))
            .map_err(|e| format!("cannot write {name}: {e}"))?;
    }
    fs::canonicalize(&name).map_err(|e| format!("cannot open {name}: {e}"))
}

/// A tab's label: the page's first Markdown heading, else the last part of
/// its address.
fn get_heading(location: &str, contents: &str) -> String {
    let mut heading = String::new();
    let mut in_heading = false;
    let parser = Parser::new_ext(contents, Options::empty());
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
        let end_part = location.split('/').next_back().unwrap_or("New Tab");
        heading = end_part.to_string();
    }

    heading
}

fn is_wasm(address: &str) -> bool {
    address.ends_with(".wasm")
}

fn is_local_file(address: &str) -> bool {
    address.starts_with("file://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_told_apart() {
        assert_eq!(page_kind("file:///home/me/qemu-system-x86_64.wasm"), PageKind::Wasm);
        assert_eq!(page_kind("https://example.com/app.wasm"), PageKind::Wasm);
        assert_eq!(page_kind(START_PAGE), PageKind::Markdown);
        assert_eq!(page_kind("file:///home/me/notes.txt"), PageKind::Markdown);
        assert_eq!(page_kind("https://example.com/README.MD?plain=1"), PageKind::Markdown);
        assert_eq!(page_kind("https://example.com/"), PageKind::Web);
        assert_eq!(page_kind("example.com"), PageKind::Web);
        assert_eq!(page_kind(WEB_SETUP), PageKind::WebSetup);
        assert_eq!(web_label("https://www.example.com/a/b?c"), "www.example.com");
        assert_eq!(web_label("example.com"), "example.com");
        assert_eq!(page_kind("file:///home/me/ticker-20261110T120000Z-1-0123456789ab.wasmsavestate"), PageKind::SavedState);
    }

    #[test]
    fn the_session_file_round_trips() {
        let session = SessionFile {
            version: SESSION_VERSION,
            current: "0123456789ab".into(),
            web_checkpoint: Some(PathBuf::from("/m/qemu-system-x86_64-20261110T120000Z-1-ba9876543210.wasmsavestate")),
            tabs: vec![SessionTab {
                id: "0123456789ab".into(),
                label: "m-browser".into(),
                location: START_PAGE.into(),
                back: vec!["https://example.com/".into()],
                forward: vec![],
                scroll: 120.5,
                saved_state: None,
            }],
        };
        let json = serde_json::to_string_pretty(&session).unwrap();
        let back: SessionFile = serde_json::from_str(&json).unwrap();
        assert_eq!((back.current, back.web_checkpoint), (session.current, session.web_checkpoint));
        assert_eq!(back.tabs[0].back, session.tabs[0].back);
        assert_eq!(back.tabs[0].scroll, 120.5);
        // Fields added later default when an older file lacks them.
        let old: SessionFile = serde_json::from_str(
            r#"{"version": 1, "current": "x", "web_checkpoint": null,
                "tabs": [{"id": "x", "label": "New Tab", "location": "https://example.com/"}]}"#,
        )
        .unwrap();
        assert_eq!((old.tabs[0].scroll, old.tabs[0].saved_state.clone()), (0.0, None));
        let tab = Tab::restored(3, old.tabs.into_iter().next().unwrap());
        assert!(!tab.loaded && tab.program.is_none() && tab.restore_scroll.is_none());
        assert_eq!((tab.identifier, tab.id.as_str()), (3, "x"));
    }

    #[test]
    fn only_a_tabs_own_saved_states_are_removed() {
        let dir = crate::saved_state::tests::TestDir::new("own-states");
        let own = dir.0.join("ticker-20261110T120000Z-2-0123456789ab.wasmsavestate");
        let other = dir.0.join("ticker-20261110T120000Z-2-ba9876543210.wasmsavestate");
        std::fs::write(&own, b"x").unwrap();
        std::fs::write(&other, b"x").unwrap();
        remove_own_state(&other, "0123456789ab");
        remove_own_state(&own, "0123456789ab");
        assert!(!own.exists(), "the tab's own state is removed");
        assert!(other.exists(), "another tab's (or an opened page's) state is kept");
        remove_own_state(&own, "0123456789ab"); // already gone: no complaint
    }
}
