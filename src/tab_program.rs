//! A tab's program: the wasm module a `.wasm` page runs. It pauses while
//! its tab is in the background and goes on where it was when the tab
//! comes back, so each tab keeps its own running program, as it keeps its
//! own page.
//!
//! ```text
//! module_kind()         runs with                 shows         paused by
//! QEMU (core module)    wasm::run_wasm_module     child window  its monitor: stop, cont
//! component (wasi-gfx)  wasm::Wasm::run_wasm      child window  epochs, and no frames
//! other core module     wasm_headless::start      text page     its next save point
//! ```
//!
//! Each program that draws gets a child window of its own and keeps it,
//! hidden, while in the background, so it never has to set up its drawing
//! again; QEMU draws with softbuffer and components with wgpu, which could
//! not share a window anyway.

use std::fs::{self, File};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use winit::event::{ElementState, WindowEvent};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::qemu_gfx::{self, GfxEventQueue, InputEvent};
use crate::qemu_save::{QemuControl, Report, Request, Saving};
use crate::wasm::{ComponentControl, Wasm};
use crate::wasm_headless::{self, Command, Control, Outcome, OutputPage};
use crate::winit_wasi::{MyWindowWrapper, WinitEventToSurfaceProxy};

/// What kind of program a wasm file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleKind {
    /// QEMU (qemu-wasi): a core module that imports m-browser's JIT or
    /// display functions, or exports QEMU's main-loop functions.
    Qemu,
    /// A component, such as a wasi-gfx app.
    Component,
    /// Any other core module: a WASI command, shown as a text page.
    Core,
}

impl ModuleKind {
    /// Whether it draws into a child window.
    pub fn needs_window(self) -> bool {
        self != ModuleKind::Core
    }
}

/// Tell QEMU, components and other core modules apart by their imports and
/// exports, which come before any code in a module.
pub fn module_kind(path: &Path) -> Result<ModuleKind> {
    use wasmparser::{Encoding, Parser, Payload};

    let wasm = fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut parser = Parser::new(0);
    parser.set_features(wasmparser::WasmFeatures::all());
    for payload in parser.parse_all(&wasm) {
        match payload.with_context(|| format!("{} is not a wasm module", path.display()))? {
            Payload::Version { encoding: Encoding::Component, .. } => return Ok(ModuleKind::Component),
            Payload::ImportSection(imports) => {
                for import in imports {
                    let import = import?;
                    if import.module == "env"
                        && (import.name.starts_with("wasi_jit_") || import.name.starts_with("host_gfx_"))
                    {
                        return Ok(ModuleKind::Qemu);
                    }
                }
            }
            Payload::ExportSection(exports) => {
                for export in exports {
                    if export?.name.starts_with("wasi_qemu_") {
                        return Ok(ModuleKind::Qemu);
                    }
                }
            }
            Payload::CodeSectionStart { .. } => break,
            _ => {}
        }
    }
    Ok(ModuleKind::Core)
}

/// The page area of the main window, in physical pixels relative to its
/// top-left corner: where a tab's child window goes. Physical, because a
/// new child window may not know its scale factor yet, and a guess that
/// differs from the main window's would size it wrongly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Move and size a child window to `area`, unless it is there already.
fn place_window(window: &Window, placed: &mut Option<Area>, area: Area) {
    if *placed == Some(area) {
        return;
    }
    // A child window's position is relative to its parent (X11).
    window.set_outer_position(winit::dpi::PhysicalPosition::new(area.x, area.y));
    let _ = window.request_inner_size(winit::dpi::PhysicalSize::new(area.width, area.height));
    *placed = Some(area);
}

/// Move a child window to the top-left corner of `area`, keeping its size,
/// unless it is there already.
fn move_window(window: &Window, placed: &mut Option<(i32, i32)>, area: Area) {
    if *placed == Some((area.x, area.y)) {
        return;
    }
    window.set_outer_position(winit::dpi::PhysicalPosition::new(area.x, area.y));
    *placed = Some((area.x, area.y));
}

/// A program's state in words, set by its thread, for the status bar.
#[derive(Clone)]
struct Status(Arc<Mutex<String>>);

impl Status {
    fn new(text: &str) -> Self {
        Status(Arc::new(Mutex::new(text.to_string())))
    }

    fn set(&self, text: impl Into<String>) {
        *self.0.lock().unwrap() = text.into();
    }

    fn get(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

/// The program of a tab.
pub enum TabProgram {
    Qemu(QemuTab),
    Component(ComponentTab),
    Core(CoreTab),
    /// It could not start; why.
    Failed(String),
}

impl TabProgram {
    /// Start the module at `path` for the tab whose id is `owner` (it names
    /// the program's saved states). `window` is its child window, for the
    /// kinds that draw (`ModuleKind::needs_window`).
    pub fn start(kind: ModuleKind, path: &Path, owner: &str, window: Option<Arc<Window>>) -> TabProgram {
        let started = match (kind, window) {
            (ModuleKind::Qemu, Some(window)) => {
                Ok(TabProgram::Qemu(QemuTab::start(QemuStart::Boot(path.to_path_buf()), owner, window)))
            }
            (ModuleKind::Component, Some(window)) => ComponentTab::start(path, window).map(TabProgram::Component),
            (ModuleKind::Core, _) => Ok(TabProgram::Core(CoreTab::start(path, owner))),
            (_, None) => Err(anyhow::anyhow!("it draws, but has no window to draw into")),
        };
        started.unwrap_or_else(|e| TabProgram::Failed(format!("{e:#}")))
    }

    /// Start QEMU for the tab `owner`, drawing into `window`: a web tab, or
    /// the setup of the web checkpoint.
    pub fn start_qemu(start: QemuStart, owner: &str, window: Arc<Window>) -> TabProgram {
        TabProgram::Qemu(QemuTab::start(start, owner, window))
    }

    /// For a web tab or a setup: the owner id its QEMU's overlays are
    /// named with (`remove_overlays`).
    pub fn overlay_owner(&self) -> Option<String> {
        match self {
            TabProgram::Qemu(qemu) if qemu.role != QemuRole::Plain => Some(qemu.owner.clone()),
            _ => None,
        }
    }

    /// What its QEMU is for, if it runs QEMU.
    pub fn qemu_role(&self) -> Option<QemuRole> {
        match self {
            TabProgram::Qemu(qemu) => Some(qemu.role),
            _ => None,
        }
    }

    /// Resume a hibernated core module (a saved state of the generic
    /// engine) in the tab.
    pub fn resume_core(path: &Path) -> TabProgram {
        TabProgram::Core(CoreTab::resume_from(path))
    }

    /// Act on what its threads reported; called on every frame for every
    /// tab. Returns the saved states written since the last call (QEMU's
    /// checkpoints, a core module's hibernation), and failures.
    pub fn tick(&mut self) -> Vec<Result<PathBuf, String>> {
        match self {
            TabProgram::Qemu(qemu) => qemu.tick(),
            TabProgram::Core(core) => core.hibernated.lock().unwrap().take().into_iter().map(Ok).collect(),
            _ => Vec::new(),
        }
    }

    /// Before the browser quits: save what can be saved for the next start.
    /// Only core modules with save points do (they hibernate; `tick` then
    /// returns the file). QEMU's tabs come back by their address: a web tab
    /// from the web checkpoint, which costs no disk space, where a
    /// checkpoint of its own would take about 1 GB. Components cannot be
    /// saved. Returns whether a saved state is coming.
    pub fn save_for_session(&mut self) -> bool {
        match self {
            TabProgram::Core(core) => core.save_for_session(),
            _ => false,
        }
    }

    /// Whether its program still runs.
    pub fn is_running(&self) -> bool {
        match self {
            TabProgram::Qemu(qemu) => !qemu.thread.is_finished(),
            TabProgram::Component(component) => !component.thread.is_finished(),
            TabProgram::Core(core) => !core.thread.is_finished(),
            TabProgram::Failed(_) => false,
        }
    }

    /// Have QEMU write a checkpoint (`tick` returns it once written).
    /// False for programs that are not QEMU.
    pub fn save_checkpoint(&mut self) -> bool {
        match self {
            TabProgram::Qemu(qemu) => {
                qemu.save();
                true
            }
            _ => false,
        }
    }

    /// A web tab's Chromium opens `url`. False if this is not a web tab, or
    /// its QEMU has ended.
    pub fn open_url(&mut self, url: &str) -> bool {
        match self {
            TabProgram::Qemu(qemu) if qemu.role == QemuRole::Web && !qemu.thread.is_finished() => {
                qemu.open_url(url);
                true
            }
            _ => false,
        }
    }

    /// Its tab went to the background: pause it and hide its window.
    pub fn pause(&mut self) {
        match self {
            TabProgram::Qemu(qemu) => qemu.pause(),
            TabProgram::Component(component) => component.pause(),
            TabProgram::Core(core) => core.pause(),
            TabProgram::Failed(_) => {}
        }
    }

    /// Its tab is back: show its window and let it go on.
    pub fn resume(&mut self) {
        match self {
            TabProgram::Qemu(qemu) => qemu.resume(),
            TabProgram::Component(component) => component.resume(),
            TabProgram::Core(core) => core.resume(),
            TabProgram::Failed(_) => {}
        }
    }

    /// Stop it for good (another page in its tab, the tab closed, the
    /// browser quitting) and hide its window, without waiting. Returns
    /// QEMU's thread, which must get time to close its disk images before
    /// m-browser exits.
    pub fn stop(self) -> Option<JoinHandle<()>> {
        match self {
            TabProgram::Qemu(qemu) => Some(qemu.stop()),
            TabProgram::Component(component) => {
                component.stop();
                None
            }
            TabProgram::Core(core) => {
                core.stop();
                None
            }
            TabProgram::Failed(_) => None,
        }
    }

    /// Its tab is the current one and the page area is `area`. QEMU's
    /// window covers it and follows its size (QEMU scales its picture to
    /// the window). A component's window only follows its position: a
    /// component draws at the size its window had when it started, since
    /// wasi-webgpu configures its swap chain once and never again, so a
    /// resized window would keep an unpainted part.
    pub fn place(&mut self, area: Area) {
        match self {
            TabProgram::Qemu(qemu) => qemu.place(area),
            TabProgram::Component(component) => move_window(&component.window, &mut component.placed, area),
            TabProgram::Core(_) | TabProgram::Failed(_) => {}
        }
    }

    /// What the status bar says about it.
    pub fn status(&self) -> String {
        match self {
            TabProgram::Qemu(qemu) => qemu.status(),
            TabProgram::Component(component) => component.status(),
            TabProgram::Core(core) => core.status(),
            TabProgram::Failed(message) => format!("cannot run this page: {message}"),
        }
    }

    /// Pass on a window event while its tab is the active one. Pointer
    /// events count over the program's own window. Keyboard events come
    /// from the main window (a child window never has the keyboard focus)
    /// and are passed on when `keys` is set: while no egui text field,
    /// such as the address bar, wants them.
    pub fn handle_event(&mut self, window_id: WindowId, event: &WindowEvent, keys: bool) {
        match self {
            TabProgram::Qemu(qemu) => qemu.handle_event(window_id, event, keys),
            TabProgram::Component(component) => component.handle_event(window_id, event, keys),
            TabProgram::Core(_) | TabProgram::Failed(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------
//  QEMU
// ---------------------------------------------------------------------------

/// The disk image the browser's QEMU boots, relative to the working directory
/// (QEMU sees it as /data/m.qcow2).
pub const QEMU_DISK_IMAGE: &str = "m.qcow2";

/// How long a new QEMU waits for its disk image while another QEMU holds
/// it, typically one that is quitting because its tab was closed or went
/// to another page: longer than QEMU gets to quit (10 s, `QUIT_TIMEOUT` in
/// wasm.rs).
const DISK_WAIT: Duration = Duration::from_secs(12);

/// How a tab's QEMU starts.
pub enum QemuStart {
    /// Boot the QEMU module at this path on `m.qcow2`: a `.wasm` page.
    Boot(PathBuf),
    /// Boot it on a new copy of `m.qcow2`, made first: the tab in which the
    /// web checkpoint is made ("Set up web tabs").
    Setup(PathBuf),
    /// Resume a checkpoint, then, if given, have the guest's Chromium open
    /// an address: a web tab.
    Resume { checkpoint: PathBuf, url: Option<String> },
}

/// What a tab's QEMU is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QemuRole {
    /// `qemu-system-x86_64.wasm` as a page, on `m.qcow2`.
    Plain,
    /// The web checkpoint's setup, on a copy of `m.qcow2`.
    Setup,
    /// A web tab: Chromium in QEMU, from a checkpoint.
    Web,
}

/// QEMU in a tab.
pub struct QemuTab {
    role: QemuRole,
    /// Created hidden: shown once QEMU has started (a resumed QEMU: once
    /// its checkpoint is loaded), while its tab is the current one. Until
    /// then the page area shows the tab's status.
    window: Arc<Window>,
    /// Keyboard and pointer events for QEMU, and the window's size and
    /// repaint requests for its display (see qemu_gfx.rs).
    queue: GfxEventQueue,
    control: QemuControl,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    status: Status,
    paused: bool,
    /// Set once QEMU runs: by the thread when it starts QEMU, or by `tick`
    /// when a resumed QEMU has loaded its checkpoint.
    started: Arc<AtomicBool>,
    /// An address for the guest's Chromium to open once QEMU has resumed.
    url: Option<String>,
    /// The latest of QEMU's reports, for the status bar.
    note: Option<String>,
    /// Whether the tab is the current one.
    active: bool,
    /// Whether the window is shown.
    visible: bool,
    /// Where the window was put last (`place_window`).
    placed: Option<Area>,
    /// The tab's id: the names of the overlays its QEMU writes end with it.
    owner: String,
    /// A setup's copy of `m.qcow2`, once made.
    disk: Arc<Mutex<Option<String>>>,
    /// Whether QEMU wrote a checkpoint (a setup's copy is kept then).
    saved_any: bool,
}

impl QemuTab {
    fn start(start: QemuStart, owner: &str, window: Arc<Window>) -> QemuTab {
        let surface = wasi_surface_wasmtime::Surface::new(Box::new(MyWindowWrapper(window.clone())));
        let queue = qemu_gfx::new_event_queue();
        // QEMU's display scales its frames to the window (qemu_gfx.rs).
        let size = window.inner_size();
        qemu_gfx::set_window_size(&queue, size.width, size.height);
        let control = QemuControl::new();
        let stop = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let (role, url, first_status) = match &start {
            QemuStart::Boot(_) => (QemuRole::Plain, None, "starting QEMU"),
            QemuStart::Setup(_) => (QemuRole::Setup, None, "preparing the web checkpoint's disk"),
            QemuStart::Resume { url, .. } => (QemuRole::Web, url.clone(), "resuming the web checkpoint"),
        };
        let status = Status::new(first_status);
        let disk = Arc::new(Mutex::new(None));
        let thread = {
            let owner = owner.to_string();
            let (queue, control, stop, status, started, disk) =
                (queue.clone(), control.clone(), stop.clone(), status.clone(), started.clone(), disk.clone());
            std::thread::spawn(move || {
                let ended = run_tab_qemu(start, owner, surface, queue, control, stop, &status, &started, &disk);
                match ended {
                    Ok(()) => {
                        println!("QEMU core module ran successfully");
                        status.set("QEMU has exited");
                    }
                    Err(message) => {
                        eprintln!("{message}");
                        status.set(message);
                    }
                }
            })
        };
        // Unlike a wasi-gfx component, QEMU takes no events from its
        // surface: input reaches it through `queue`, and it needs no
        // animation frames. So nothing sends to the surface. That matters
        // when QEMU exits: dropping the surface closes its event channels,
        // and wasi-gfx-runtime panics on a send to a closed channel.
        QemuTab {
            role,
            window,
            queue,
            control,
            stop,
            thread,
            status,
            paused: false,
            started,
            url,
            note: None,
            active: true,
            visible: false,
            placed: None,
            owner: owner.to_string(),
            disk,
            saved_any: false,
        }
    }

    /// Show the window while the tab is the current one and QEMU has
    /// started; QEMU paints all of it (black until its first frame).
    fn update_visibility(&mut self) {
        let show = self.active && self.started.load(Ordering::Relaxed);
        if show != self.visible {
            self.window.set_visible(show);
            self.visible = show;
            if show {
                // A hidden window has lost its picture.
                qemu_gfx::request_repaint(&self.queue);
            }
        }
    }

    fn place(&mut self, area: Area) {
        place_window(&self.window, &mut self.placed, area);
        self.update_visibility();
    }

    /// Act on what QEMU reported: once a web tab's QEMU has loaded its
    /// checkpoint, show it and type the address. Returns the checkpoints
    /// QEMU wrote, and failures.
    fn tick(&mut self) -> Vec<Result<PathBuf, String>> {
        let mut saved = Vec::new();
        for report in self.control.reports() {
            match report {
                Report::Resumed => {
                    self.started.store(true, Ordering::Relaxed);
                    self.status.set("QEMU is running");
                    self.note = None;
                    if let Some(url) = self.url.take() {
                        self.type_url(&url);
                    }
                }
                Report::Saved(path) => {
                    self.note = Some(format!("saved a checkpoint: {}", path.display()));
                    self.saved_any = true;
                    saved.push(Ok(path));
                }
                Report::Failed(message) => {
                    self.note = Some(message.clone());
                    saved.push(Err(message));
                }
            }
        }
        self.update_visibility();
        saved
    }

    /// Have the guest's Chromium open `url`: Ctrl+L moves the keyboard to
    /// its address bar and selects what is there; then the address, and
    /// Enter. QEMU's display delivers the keys a few at a time.
    fn type_url(&mut self, url: &str) {
        use crate::qemu_keymap::{chord_events, text_events, KEY_L, KEY_LEFTCTRL};
        let mut events = chord_events(KEY_LEFTCTRL, KEY_L);
        events.extend(text_events(&format!("{url}\n")));
        if let Ok(mut queue) = self.queue.lock() {
            queue.extend(events);
        }
        self.note = Some(format!("opening {url}"));
    }

    /// A web tab goes to another address: its Chromium opens it, now or
    /// once QEMU has resumed.
    fn open_url(&mut self, url: &str) {
        if self.started.load(Ordering::Relaxed) {
            self.type_url(url);
        } else {
            self.url = Some(url.to_string());
        }
    }

    fn save(&mut self) {
        self.control.send(Request::Save);
        self.note = Some("saving a checkpoint".to_string());
    }

    /// `stop`: the guest's processors halt and its clocks stand still.
    /// QEMU's main loop idles meanwhile (no measurable CPU time).
    fn pause(&mut self) {
        if !self.paused {
            self.control.send(Request::Pause);
            self.paused = true;
        }
        self.active = false;
        self.update_visibility();
    }

    fn resume(&mut self) {
        self.active = true;
        self.update_visibility();
        if self.paused {
            self.control.send(Request::Continue);
            self.paused = false;
        }
        let size = self.window.inner_size();
        qemu_gfx::set_window_size(&self.queue, size.width, size.height);
        qemu_gfx::request_repaint(&self.queue);
    }

    /// Ask QEMU to quit: it powers the guest off and closes its disk
    /// images, on its own thread (`run_qemu_reactor` in wasm.rs). A setup
    /// that saved no checkpoint leaves a copy of `m.qcow2` that nothing
    /// needs: it is deleted (QEMU may still write it while quitting: Linux
    /// keeps a deleted file's data until it is closed). The overlays are
    /// the browser's to delete (`remove_overlays`), as it knows which files
    /// the web checkpoint uses.
    fn stop(mut self) -> JoinHandle<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.window.set_visible(false);
        if self.control.reports().iter().any(|r| matches!(r, Report::Saved(_))) {
            self.saved_any = true;
        }
        if self.role == QemuRole::Setup && !self.saved_any {
            if let Some(copy) = self.disk.lock().unwrap().take() {
                remove_file(Path::new(&copy), "the copy of a setup that saved no checkpoint");
            }
        }
        self.thread
    }

    fn status(&self) -> String {
        if self.thread.is_finished() {
            self.status.get()
        } else if self.paused {
            "QEMU is paused".into()
        } else {
            self.note.clone().unwrap_or_else(|| self.status.get())
        }
    }

    fn handle_event(&mut self, window_id: WindowId, event: &WindowEvent, keys: bool) {
        if self.thread.is_finished() {
            return; // nobody reads the queue any more
        }
        let mut events: Vec<(i32, i32, i32)> = Vec::new();

        // The mouse, while the pointer is over QEMU's window. Positions are
        // in window pixels; QEMU's display maps them into its frame, which
        // it scales to the window (qemu_gfx.rs), and the guest side maps
        // them to its absolute pointing device (QEMU's usb-tablet). Button
        // codes are QEMU's InputButton values.
        if window_id == self.window.id() {
            match event {
                WindowEvent::Resized(size) => qemu_gfx::set_window_size(&self.queue, size.width, size.height),
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
                        events.push((2, code, matches!(state, ElementState::Pressed) as i32));
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
        }

        if let (true, WindowEvent::KeyboardInput { event: key, .. }) = (keys, event) {
            let down = matches!(key.state, ElementState::Pressed);
            if let PhysicalKey::Code(code) = key.physical_key {
                match crate::qemu_keymap::keycode_to_evdev(code) {
                    // A key, as an evdev code.
                    Some(evdev) => events.push((3, evdev, down as i32)),
                    None => eprintln!("[qemu-gfx] no evdev mapping for {code:?}, ignoring"),
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
                        // Text, as a code point.
                        events.extend(text.chars().map(|ch| (5, ch as i32, 0)));
                    }
                }
            }
        }

        if !events.is_empty() {
            if let Ok(mut queue) = self.queue.lock() {
                queue.extend(events.into_iter().map(|(event_type, code, value)| InputEvent { event_type, code, value }));
            }
        }
    }
}

/// The QEMU thread of a tab: take the disk image's lock (and make the copy
/// a setup needs), or check the checkpoint to resume; then run QEMU until it
/// exits or is stopped.
#[allow(clippy::too_many_arguments)]
fn run_tab_qemu(
    start: QemuStart,
    owner: String,
    surface: wasi_surface_wasmtime::Surface,
    queue: GfxEventQueue,
    control: QemuControl,
    stop: Arc<AtomicBool>,
    status: &Status,
    started: &AtomicBool,
    disk: &Mutex<Option<String>>,
) -> Result<(), String> {
    // QEMU_NIC replaces the network option, e.g. to forward a host port
    // into the guest: QEMU_NIC=user,hostfwd=tcp:127.0.0.1:2222-:22
    let nic = std::env::var("QEMU_NIC").unwrap_or_else(|_| "user".into());
    // The locks are held, by name, until QEMU has quit.
    let (module, args, preopens, resume, _image_lock) = match start {
        QemuStart::Boot(module) => {
            let lock = lock_disk_image(QEMU_DISK_IMAGE, &stop, status)?;
            (module, boot_args(QEMU_DISK_IMAGE, &nic), default_preopens(), None, lock)
        }
        QemuStart::Setup(module) => {
            let copy = copy_disk(&stop, status)?;
            *disk.lock().unwrap() = Some(copy.clone());
            let lock = lock_disk_image(&copy, &stop, status)?;
            (module, boot_args(&copy, &nic), default_preopens(), None, lock)
        }
        QemuStart::Resume { checkpoint, .. } => {
            // The checkpoint's disks are frozen: this QEMU writes an
            // overlay of its own (qemu_save.rs), so no lock is needed.
            let resume = crate::qemu_save::prepare_resume(&checkpoint)
                .map_err(|e| format!("cannot resume {}: {e:#}", checkpoint.display()))?;
            (resume.module.clone(), resume.args.clone(), resume.preopens.clone(), Some(resume), None)
        }
    };
    if resume.is_none() {
        status.set("QEMU is running");
        started.store(true, Ordering::Relaxed);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let preopens: Vec<(&str, &str)> = preopens.iter().map(|(h, g)| (h.as_str(), g.as_str())).collect();
    let saving = Saving { control, resume, owner: Some(owner) };
    crate::wasm::run_wasm_module(
        &module.to_string_lossy(),
        &args,
        &preopens,
        Some(surface),
        queue,
        Some(stop),
        Some(saving),
    )
    .map_err(|e| format!("QEMU failed: {e:#}"))
}

/// The directories QEMU gets: `./pc-bios` as `/bios`, and the working
/// directory (the m-browser directory) as `/data`.
fn default_preopens() -> Vec<(String, String)> {
    vec![("pc-bios".into(), "/bios".into()), (".".into(), "/data".into())]
}

/// QEMU's arguments for booting `disk`, a qcow2 image in the working
/// directory, with the network option `nic`.
fn boot_args(disk: &str, nic: &str) -> Vec<String> {
    [
        "qemu-system-x86_64",
        "-L", "/bios",
        "-drive", &format!("file=/data/{disk},format=qcow2"),
        // With QEMU's default of 128 MiB the guest's X desktop swaps
        // constantly: everything crawls, and X stalls long enough for the
        // guest kernel to drop keystrokes. With 512 MiB, starting Chromium
        // still pushed ~100 MiB to swap and kept the kernel busy reclaiming
        // and compacting memory (a sixth of the guest's CPU time); with
        // 1 GiB none of that happens. (QEMU's memory, which includes this,
        // must stay well under wasm32's 4 GiB.)
        "-m", "1024",
        // 256 MiB of translated code. When the buffer fills up, QEMU
        // discards all translations and has to translate (and the host
        // recompile) its working set again; with 64 MiB that happened every
        // few seconds while the desktop started, and it reached the desktop
        // a minute later. Costs ~400 MiB more host memory.
        "-accel", "tcg,tb-size=256",
        // A USB tablet reports absolute positions, so the guest pointer
        // follows the host pointer exactly (needed for X). XHCI rather than
        // UHCI: no 1 kHz polling timer.
        "-device", "qemu-xhci",
        "-device", "usb-tablet",
        // The window shows the guest's screen. These add QEMU's monitor
        // (Ctrl+Alt+2) and the guest's serial port (Ctrl+Alt+3) as text
        // consoles; Ctrl+Alt+1 switches back.
        "-monitor", "vc",
        "-serial", "vc",
        // User-mode networking, provided by m-browser (see qemu_net.rs): the
        // guest gets 10.0.2.15 by DHCP, and its TCP, UDP and pings go out
        // through host sockets.
        "-nic", nic,
    ]
    .iter()
    .map(|a| a.to_string())
    .collect()
}

/// Copy `m.qcow2` to a new file, `m-web-<UTC time>.qcow2`, for the web
/// checkpoint. Web tabs must find their disk exactly as it was saved, while
/// `m.qcow2` is the user's own, which the plain QEMU page goes on writing;
/// and a new name per setup keeps an older checkpoint's disk intact.
/// `m.qcow2`'s lock is held while copying, so that no QEMU writes it
/// meanwhile; the copy is never written to `m.qcow2`'s side.
fn copy_disk(stop: &AtomicBool, status: &Status) -> Result<String, String> {
    let lock = lock_disk_image(QEMU_DISK_IMAGE, stop, status)?;
    if lock.is_none() && !Path::new(QEMU_DISK_IMAGE).is_file() {
        return Err(format!("there is no {QEMU_DISK_IMAGE} to copy (start m-browser in its directory)"));
    }
    let name = format!("m-web-{}.qcow2", crate::saved_state::utc_stamp(crate::saved_state::unix_now()));
    status.set(format!("copying {QEMU_DISK_IMAGE} to {name}, once for this setup"));
    let tmp = format!("{name}.tmp");
    if let Err(e) = fs::copy(QEMU_DISK_IMAGE, &tmp).and_then(|_| fs::rename(&tmp, &name)) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot copy {QEMU_DISK_IMAGE} to {name}: {e}"));
    }
    Ok(name)
}

/// Delete a file m-browser made, saying why.
pub fn remove_file(path: &Path, what: &str) {
    match fs::remove_file(path) {
        Ok(()) => println!("Removed {}, {what}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("Cannot remove {} ({e})", path.display()),
    }
}

/// Delete the disk overlays a tab's QEMU wrote: the files in the save
/// directory whose names end with `-<owner>.disk<N>.qcow2` (qemu_save.rs
/// names them so), except those in `keep`, the images the web checkpoint
/// is made of: a setup that saved twice froze its first overlay into the
/// second checkpoint. The owner is the tab's own id, so no other tab's
/// files match.
pub fn remove_overlays(owner: &str, keep: &[PathBuf]) {
    remove_overlays_in(&crate::qemu_save::Session::default_save_dir(), owner, keep);
}

fn remove_overlays_in(dir: &Path, owner: &str, keep: &[PathBuf]) {
    let marker = format!("-{owner}.disk");
    let keep: Vec<PathBuf> = keep.iter().filter_map(|p| fs::canonicalize(p).ok()).collect();
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !(name.contains(&marker) && name.ends_with(".qcow2")) {
            continue;
        }
        if fs::canonicalize(entry.path()).is_ok_and(|path| keep.contains(&path)) {
            continue;
        }
        remove_file(&entry.path(), "a disk overlay of a closed tab that nothing needs");
    }
}

/// Whether m-browser made this disk image for web tabs: a setup's copy of
/// `m.qcow2` or one of its overlays. Never `m.qcow2` itself.
pub fn is_web_disk(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name != QEMU_DISK_IMAGE && name.ends_with(".qcow2") && (name.starts_with("m-web-") || name.contains(".disk"))
}

/// Lock `path` for as long as the returned file stays open, so that no other
/// QEMU started by an m-browser writes the same disk image at the same time:
/// two writers corrupt a qcow2 image, and QEMU on WASI cannot lock files
/// itself. Waits up to `DISK_WAIT` for another QEMU to let go of it, unless
/// `stop` is set meanwhile.
fn lock_disk_image(path: &str, stop: &AtomicBool, status: &Status) -> Result<Option<File>, String> {
    let file = match fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        // Let QEMU report a missing or unreadable image.
        Err(_) => return Ok(None),
    };
    let deadline = Instant::now() + DISK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) => {
                if stop.load(Ordering::Relaxed) {
                    return Err(format!("QEMU stopped before it could use {path}"));
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{path} is in use by another QEMU (in another tab or another m-browser); \
                         not starting a second one on the same disk image"
                    ));
                }
                status.set(format!("waiting for {path}, which another QEMU is using"));
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(fs::TryLockError::Error(e)) => {
                eprintln!("Cannot lock {path} ({e}); starting QEMU anyway");
                return Ok(None);
            }
        }
    }
}

// ---------------------------------------------------------------------------
//  wasi-gfx components
// ---------------------------------------------------------------------------

/// A component in a tab, drawing with wasi-gfx.
pub struct ComponentTab {
    window: Arc<Window>,
    control: Arc<ComponentControl>,
    /// Forwards window events; dropped if forwarding ever panics.
    events: Option<WinitEventToSurfaceProxy>,
    thread: JoinHandle<()>,
    status: Status,
    /// Where the window was moved last (`move_window`). Its size never
    /// changes (see `TabProgram::place`).
    placed: Option<(i32, i32)>,
}

impl ComponentTab {
    fn start(path: &Path, window: Arc<Window>) -> Result<ComponentTab> {
        let control = ComponentControl::new()?;
        let surface = wasi_surface_wasmtime::Surface::new(Box::new(MyWindowWrapper(window.clone())));
        let proxy = surface.proxy();
        let status = Status::new("running");
        // Set once the component has ended and dropped its surface: from
        // then on, wasi-gfx-runtime panics on any event or frame for it.
        let finished = Arc::new(AtomicBool::new(false));
        let thread = {
            let path = path.to_string_lossy().into_owned();
            let (control, status, finished) = (control.clone(), status.clone(), finished.clone());
            std::thread::spawn(move || {
                // wasi-gfx-runtime panics in some situations instead of
                // returning an error (a swap chain without a texture, for
                // one): report that too, instead of a stale status.
                let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    Wasm::new(&control).and_then(|mut wasm| pollster::block_on(wasm.run_wasm(&path, surface)))
                }));
                finished.store(true, Ordering::Relaxed);
                match result {
                    Ok(Ok(())) => status.set("the program has ended"),
                    Ok(Err(_)) if control.is_stopped() => status.set("stopped"),
                    Ok(Err(e)) => {
                        eprintln!("Error running wasm: {e:#}");
                        status.set(format!("the program failed: {e:#}"));
                    }
                    Err(_) => status.set("the program stopped: m-browser's wasi-gfx host code panicked (see the terminal)"),
                }
            })
        };
        // Animation frames, about 60 a second, while it runs and is not
        // paused: a paused component that waits for its next frame (as
        // drawing programs usually do) simply gets none.
        {
            let (proxy, control) = (proxy.clone(), control.clone());
            std::thread::spawn(move || loop {
                if finished.load(Ordering::Relaxed) {
                    break;
                }
                let stopped = control.is_stopped();
                // After a stop, one last frame wakes a component that waits
                // for one, so that it reaches its epoch check and traps.
                if (stopped || !control.is_paused())
                    && std::panic::catch_unwind(AssertUnwindSafe(|| proxy.animation_frame())).is_err()
                {
                    break;
                }
                if stopped {
                    break;
                }
                std::thread::sleep(Duration::from_millis(16));
            });
        }
        Ok(ComponentTab {
            window,
            control,
            events: Some(WinitEventToSurfaceProxy::new(proxy)),
            thread,
            status,
            placed: None,
        })
    }

    fn pause(&mut self) {
        self.control.pause();
        self.window.set_visible(false);
    }

    fn resume(&mut self) {
        self.window.set_visible(true);
        self.control.resume();
    }

    fn stop(self) {
        self.control.stop();
        self.window.set_visible(false);
    }

    fn status(&self) -> String {
        if self.control.is_paused() && !self.thread.is_finished() {
            "paused".into()
        } else {
            self.status.get()
        }
    }

    fn handle_event(&mut self, window_id: WindowId, event: &WindowEvent, keys: bool) {
        let ours = window_id == self.window.id();
        let pass = match event {
            WindowEvent::CursorMoved { .. } | WindowEvent::MouseInput { .. } | WindowEvent::Resized(_) => ours,
            WindowEvent::KeyboardInput { .. } => keys,
            WindowEvent::ModifiersChanged(_) => true,
            _ => false,
        };
        if !pass || self.thread.is_finished() {
            return;
        }
        // wasi-gfx-runtime panics when an event channel is full (a
        // component that does not read its events) or closed (one that has
        // ended): stop forwarding then, rather than take the browser down.
        let failed = self.events.as_mut().is_some_and(|events| {
            std::panic::catch_unwind(AssertUnwindSafe(|| events.send_event(event))).is_err()
        });
        if failed {
            self.events = None;
        }
    }
}

// ---------------------------------------------------------------------------
//  Other core modules
// ---------------------------------------------------------------------------

/// Another core module in a tab: a WASI command, run as by
/// `m-browser --wasm-headless`, its output on a text page. It gets no
/// directories, environment or arguments (argv[0] is its file name), and
/// no standard input.
pub struct CoreTab {
    control: Arc<Control>,
    output: OutputPage,
    thread: JoinHandle<()>,
    status: Status,
    paused: bool,
    /// The file the program hibernated to, until `tick` hands it over.
    hibernated: Arc<Mutex<Option<PathBuf>>>,
}

/// Where hibernated states go: the working directory (the m-browser
/// directory), as all of m-browser's saved states do.
fn save_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

impl CoreTab {
    fn start(path: &Path, owner: &str) -> CoreTab {
        let options = wasm_headless::Options {
            module: path.to_path_buf(),
            args: Vec::new(),
            env: Vec::new(),
            preopens: Vec::new(),
            step: None,
            save_dir: save_dir(),
            output: None,
            owner: Some(owner.to_string()),
        };
        CoreTab::spawn(move |control, output| {
            wasm_headless::start(&wasm_headless::Options { output: Some(output), ..options }, control)
        })
    }

    /// Resume a hibernated program (a `--wasm-headless` saved state).
    fn resume_from(path: &Path) -> CoreTab {
        let path = path.to_path_buf();
        CoreTab::spawn(move |control, output| wasm_headless::resume(&path, &save_dir(), control, Some(output)))
    }

    fn spawn(
        run: impl FnOnce(Arc<Control>, OutputPage) -> Result<Outcome> + Send + 'static,
    ) -> CoreTab {
        let control = Control::new();
        let output = OutputPage::new();
        let status = Status::new("running");
        let hibernated = Arc::new(Mutex::new(None));
        let thread = {
            let (control, output, status, hibernated) =
                (control.clone(), output.clone(), status.clone(), hibernated.clone());
            std::thread::spawn(move || {
                let ended = match run(control, output.clone()) {
                    Ok(Outcome::Exited(code)) => format!("exited with status {code}"),
                    Ok(Outcome::Hibernated(file)) => {
                        let ended = format!("saved to {} and stopped", file.display());
                        *hibernated.lock().unwrap() = Some(file);
                        ended
                    }
                    Ok(Outcome::Quit) => "stopped".to_string(),
                    Err(e) => format!("failed: {e:#}"),
                };
                output.note(&ended);
                status.set(ended);
            })
        };
        CoreTab { control, output, thread, status, paused: false, hibernated }
    }

    /// Hibernate for the next start, if the program can (it has save
    /// points); it does so at its next save point. Returns whether it was
    /// asked to.
    fn save_for_session(&mut self) -> bool {
        if self.thread.is_finished() || !self.control.can_hibernate() {
            return false;
        }
        self.control.send(Command::Hibernate);
        // A paused program must go on to reach its next save point.
        self.control.send(Command::Continue);
        true
    }

    /// The program's output so far.
    pub fn output(&self) -> &OutputPage {
        &self.output
    }

    /// It pauses at its next save point: its next call to one of the WASI
    /// functions saved_state::wasi wraps. One that never calls them (a
    /// pure computation) runs on.
    fn pause(&mut self) {
        self.control.send(Command::Pause);
        self.paused = true;
    }

    fn resume(&mut self) {
        self.control.send(Command::Continue);
        self.paused = false;
    }

    fn stop(self) {
        self.control.send(Command::Quit);
    }

    fn status(&self) -> String {
        if self.paused && !self.thread.is_finished() {
            "paused".into()
        } else {
            self.status.get()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saved_state::tests::TestDir;

    #[test]
    fn modules_are_told_apart() {
        let dir = TestDir::new("module-kind");
        let kind = |name: &str, wat: &str| {
            let path = dir.0.join(name);
            fs::write(&path, wat::parse_str(wat).unwrap()).unwrap();
            module_kind(&path).unwrap()
        };
        assert_eq!(kind("qemu-jit.wasm", r#"(module (import "env" "wasi_jit_compile" (func)))"#), ModuleKind::Qemu);
        assert_eq!(kind("qemu-gfx.wasm", r#"(module (import "env" "host_gfx_init" (func (param i32 i32))))"#), ModuleKind::Qemu);
        assert_eq!(
            kind("qemu-step.wasm", r#"(module (func (export "wasi_qemu_step") (result i32) i32.const 0))"#),
            ModuleKind::Qemu
        );
        assert_eq!(
            kind("hello.wasm", r#"(module (import "wasi_snapshot_preview1" "proc_exit" (func (param i32))) (func (export "_start")))"#),
            ModuleKind::Core
        );
        assert_eq!(kind("app.wasm", "(component)"), ModuleKind::Component);
        let text = dir.0.join("page.md");
        fs::write(&text, "# not wasm").unwrap();
        assert!(module_kind(&text).is_err());
        assert!(!ModuleKind::Core.needs_window() && ModuleKind::Qemu.needs_window());
    }

    #[test]
    fn web_disks_are_recognized_and_overlays_kept_when_needed() {
        assert!(is_web_disk(Path::new("/m/m-web-20261110T120000Z.qcow2")));
        assert!(is_web_disk(Path::new("/m/qemu-system-x86_64-20261110T120000Z-1-0123456789ab.disk0.qcow2")));
        assert!(!is_web_disk(Path::new("/m/m.qcow2")), "never the user's own disk");
        assert!(!is_web_disk(Path::new("/m/base.raw")));
        let dir = TestDir::new("overlays");
        let file = |name: &str| {
            let path = dir.0.join(name);
            fs::write(&path, b"q").unwrap();
            path
        };
        let frozen = file("qemu-system-x86_64-20261110T120000Z-1-0123456789ab.disk0.qcow2");
        let latest = file("qemu-system-x86_64-20261110T120500Z-2-0123456789ab.disk0.qcow2");
        let other = file("qemu-system-x86_64-20261110T120500Z-0-ba9876543210.disk0.qcow2");
        remove_overlays_in(&dir.0, "0123456789ab", &[frozen.clone()]);
        assert!(frozen.exists(), "part of the web checkpoint: kept");
        assert!(!latest.exists(), "the tab's other overlay: removed");
        assert!(other.exists(), "another tab's overlay: kept");
    }
}
