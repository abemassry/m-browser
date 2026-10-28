//! A sampling profiler for QEMU running in wasmtime: where does the time go?
//!
//! Set QEMU_PROFILE=1 to use it. Every millisecond (QEMU_PROFILE_MS) a ticker
//! thread advances wasmtime's "epoch". Code compiled with epoch interruption
//! checks the epoch at every function entry and loop header; once it has
//! passed the store's deadline, wasmtime calls a callback on QEMU's own
//! thread, with QEMU's wasm code still on the stack. The callback captures
//! that stack (a `WasmBacktrace`) and charges the time since the previous
//! sample to it.
//!
//! Every QEMU_PROFILE_EVERY seconds (default 30) it prints the functions that
//! used the most time in that window:
//!   self   time in the function itself
//!   total  time in the function and everything it called
//! Code the JIT compiled from guest instructions (translation blocks, one wasm
//! module each) shows up as "[guest code]".
//!
//! QEMU calls the host (m-browser) for files, clocks, sleeping, the display,
//! the network and its JIT. A call hook times every such call. Samples are
//! also taken when a host call returns (once per tick), while the calling
//! wasm code is still on the stack, so time in host calls is shown as a
//! "[host: ...]" frame under the code that made the call. "[outside QEMU]"
//! is m-browser's own work between QEMU's main loop iterations, such as moving
//! QEMU to a fresh store.
//!
//! With QEMU_PROFILE_DIR=<dir>, each window's stacks are also written to
//! <dir>/window-NNN.folded, the input format of flame graph tools
//! (e.g. `inferno-flamegraph window-007.folded > window-007.svg`).
//!
//! Caveats: samples are taken only at function entries and loop headers, so
//! time in straight-line code is charged to the next such point. A compiled
//! translation block is mostly straight-line code, so much of its time lands
//! on the dispatch loop it returns to (tcg_qemu_tb_exec): read the two
//! together. The checks slow everything down slightly (loops most), and
//! inlined functions don't appear.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wasmtime::{AsContext, CallHook, Engine, Module, Store, UpdateDeadline, WasmBacktrace};

/// Work inside host calls that m-browser times itself, to tell it apart from
/// other host calls (WASI files, clocks and so on).
#[derive(Clone, Copy)]
pub enum HostWork {
    /// Sleeping while the guest is idle (host_net_wait).
    IdleWait = 0,
    /// Compiling a translation block's wasm module (Cranelift).
    JitCompile = 1,
    /// Everything for a JIT request, compiling included (instantiation...).
    JitTotal = 2,
    /// Showing a frame (host_gfx_update_framebuffer).
    Display = 3,
}
const HOST_WORK_KINDS: usize = 4;

/// Set once an engine has been configured for profiling: every store of
/// such an engine needs an epoch deadline, or its wasm code would trap.
static CONFIGURED: AtomicBool = AtomicBool::new(false);

/// Whether an engine was configured for profiling, so that its Stores need
/// epoch deadlines.
pub fn configured() -> bool {
    CONFIGURED.load(Ordering::Relaxed)
}

/// Advanced by the ticker together with the engine's epoch, so that a host
/// call returning can tell whether a sample is due.
static TICK: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static PROFILER: RefCell<Option<Profiler>> = const { RefCell::new(None) };
    // Updated on every wasm<->host transition, so plain Cells kept apart from
    // PROFILER: they cost next to nothing.
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static HOST_ENTERED: Cell<Option<Instant>> = const { Cell::new(None) };
    static HOST_NS: Cell<u64> = const { Cell::new(0) };
    static HOST_CALLS: Cell<u64> = const { Cell::new(0) };
    static WASM_DEPTH: Cell<u32> = const { Cell::new(0) };
    static LEFT_WASM: Cell<Option<Instant>> = const { Cell::new(None) };
    static OUTSIDE_NS: Cell<u64> = const { Cell::new(0) };
    /// TICK when the last sample was taken.
    static SAMPLED_TICK: Cell<u64> = const { Cell::new(0) };
    static WORK_NS: [Cell<u64>; HOST_WORK_KINDS] =
        const { [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)] };
}

struct Settings {
    interval: Duration,
    every: Duration,
    dir: Option<PathBuf>,
    top: usize,
}

fn settings() -> Option<Settings> {
    let on = std::env::var("QEMU_PROFILE").ok()?;
    if on.is_empty() || on == "0" {
        return None;
    }
    let num = |name: &str, default: u64| {
        std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    };
    Some(Settings {
        interval: Duration::from_millis(num("QEMU_PROFILE_MS", 1).max(1)),
        every: Duration::from_secs(num("QEMU_PROFILE_EVERY", 30).max(1)),
        dir: std::env::var_os("QEMU_PROFILE_DIR").map(PathBuf::from),
        top: num("QEMU_PROFILE_TOP", 25) as usize,
    })
}

/// Prepare an engine configuration for profiling, if QEMU_PROFILE is set.
pub fn configure(config: &mut wasmtime::Config) -> bool {
    if settings().is_none() {
        return false;
    }
    config.epoch_interruption(true);
    // A sample captures a backtrace. With backtrace details on, every frame
    // would also be looked up in QEMU's DWARF debug info, far too slow to do
    // a thousand times a second. Function names come from the name section.
    config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Disable);
    CONFIGURED.store(true, Ordering::Relaxed);
    true
}

/// Start profiling QEMU's module on this thread (QEMU's), if configured.
pub fn start(engine: &Engine, qemu: &Module) {
    let Some(settings) = settings() else { return };
    if !CONFIGURED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(dir) = &settings.dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("[qemu-profile] cannot create {}: {e}", dir.display());
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    {
        let engine = engine.clone();
        let stop = stop.clone();
        let interval = settings.interval;
        std::thread::Builder::new()
            .name("qemu-profile".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(interval);
                    TICK.fetch_add(1, Ordering::Relaxed);
                    engine.increment_epoch();
                }
            })
            .expect("cannot start the profiler's ticker thread");
    }
    eprintln!(
        "[qemu-profile] sampling every {:?}, reporting every {} s{}",
        settings.interval,
        settings.every.as_secs(),
        settings.dir.as_ref().map_or(String::new(), |d| format!(", stacks in {}", d.display()))
    );
    PROFILER.with(|p| *p.borrow_mut() = Some(Profiler::new(settings, qemu.clone(), stop)));
    ACTIVE.set(true);
}

/// Install the sampling hooks on a store that QEMU runs in. Needed for every
/// store of an engine configured for profiling.
pub fn attach<T: 'static>(store: &mut Store<T>) {
    if !CONFIGURED.load(Ordering::Relaxed) {
        return;
    }
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|ctx| {
        sample(&ctx);
        Ok(UpdateDeadline::Continue(1))
    });
    store.call_hook(|ctx, hook| {
        on_transition(&ctx, hook);
        Ok(())
    });
}

/// Stop profiling on this thread, printing the last (partial) window.
pub fn stop() {
    ACTIVE.set(false);
    if let Some(mut p) = PROFILER.with(|p| p.borrow_mut().take()) {
        p.report();
        p.stop.store(true, Ordering::Relaxed);
    }
}

/// Times host work of the given kind until dropped (only while profiling).
pub struct HostTimer {
    kind: HostWork,
    start: Option<Instant>,
}

pub fn time_host(kind: HostWork) -> HostTimer {
    HostTimer {
        kind,
        start: ACTIVE.get().then(Instant::now),
    }
}

impl Drop for HostTimer {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            add_host_work(self.kind, start.elapsed());
        }
    }
}

/// Record host work of the given kind that took `time`.
pub fn add_host_work(kind: HostWork, time: Duration) {
    if ACTIVE.get() {
        WORK_NS.with(|w| {
            let cell = &w[kind as usize];
            cell.set(cell.get() + time.as_nanos() as u64);
        });
    }
}

fn sample(store: impl AsContext) {
    SAMPLED_TICK.set(TICK.load(Ordering::Relaxed));
    PROFILER.with(|p| {
        if let Some(p) = p.borrow_mut().as_mut() {
            p.sample(store);
        }
    });
}

fn on_transition(store: impl AsContext, hook: CallHook) {
    let now = Instant::now();
    match hook {
        CallHook::CallingHost => HOST_ENTERED.set(Some(now)),
        CallHook::ReturningFromHost => {
            if let Some(entered) = HOST_ENTERED.take() {
                HOST_NS.set(HOST_NS.get() + (now - entered).as_nanos() as u64);
                HOST_CALLS.set(HOST_CALLS.get() + 1);
                // The caller's frames are still on the stack: if a sample is
                // due, take it here, so that the host call is charged to the
                // code that made it (a long call always spans a tick).
                if ACTIVE.get() && TICK.load(Ordering::Relaxed) != SAMPLED_TICK.get() {
                    sample(store);
                }
            }
        }
        CallHook::CallingWasm => {
            let depth = WASM_DEPTH.get();
            if depth == 0 {
                if let Some(left) = LEFT_WASM.take() {
                    OUTSIDE_NS.set(OUTSIDE_NS.get() + (now - left).as_nanos() as u64);
                }
            }
            WASM_DEPTH.set(depth + 1);
        }
        CallHook::ReturningFromWasm => {
            let depth = WASM_DEPTH.get().saturating_sub(1);
            WASM_DEPTH.set(depth);
            if depth == 0 {
                LEFT_WASM.set(Some(now));
            }
        }
        #[allow(unreachable_patterns)]
        _ => {}
    }
}

/// Time charged to stacks during one reporting window.
#[derive(Default)]
struct Window {
    total_ns: u64,
    samples: u64,
    host_calls: u64,
    self_ns: HashMap<Arc<str>, u64>,
    total_ns_by: HashMap<Arc<str>, u64>,
    folded: HashMap<String, u64>,
}

struct Profiler {
    settings: Settings,
    qemu: Module,
    stop: Arc<AtomicBool>,
    started: Instant,
    last: Instant,
    window_start: Instant,
    window_number: u32,
    window: Window,
    /// Names of QEMU's functions, by function index.
    names: HashMap<u32, Arc<str>>,
    /// The stack being charged, outermost frame first.
    stack: Vec<Arc<str>>,
    /// Functions already charged for the current stack (recursion).
    seen: Vec<Arc<str>>,
    guest_code: Arc<str>,
    outside: Arc<str>,
    /// "[host: ...]" frames, by kind: idle, JIT compile, JIT other, display,
    /// other host calls.
    host_frames: [Arc<str>; 5],
}

impl Profiler {
    fn new(settings: Settings, qemu: Module, stop: Arc<AtomicBool>) -> Self {
        let now = Instant::now();
        Self {
            settings,
            qemu,
            stop,
            started: now,
            last: now,
            window_start: now,
            window_number: 0,
            window: Window::default(),
            names: HashMap::new(),
            stack: Vec::new(),
            seen: Vec::new(),
            guest_code: "[guest code]".into(),
            outside: "[outside QEMU]".into(),
            host_frames: [
                "[host: idle wait]".into(),
                "[host: JIT compile]".into(),
                "[host: JIT instantiate]".into(),
                "[host: display]".into(),
                "[host: other (WASI files, clocks...)]".into(),
            ],
        }
    }

    fn sample(&mut self, store: impl AsContext) {
        let now = Instant::now();
        let gap = (now - self.last).as_nanos() as u64;
        self.last = now;

        let host = HOST_NS.take();
        let outside = OUTSIDE_NS.take();
        let work: [u64; HOST_WORK_KINDS] = WORK_NS.with(|w| std::array::from_fn(|i| w[i].take()));
        let idle = work[HostWork::IdleWait as usize];
        let compile = work[HostWork::JitCompile as usize];
        let jit_other = work[HostWork::JitTotal as usize].saturating_sub(compile);
        let display = work[HostWork::Display as usize];
        let other = host.saturating_sub(idle + compile + jit_other + display);
        let in_wasm = gap.saturating_sub(host + outside);

        self.window.samples += 1;
        self.window.host_calls += HOST_CALLS.take();

        let trace = WasmBacktrace::force_capture(store);
        self.stack.clear();
        for frame in trace.frames().iter().rev() {
            let name = if Module::same(frame.module(), &self.qemu) {
                self.names
                    .entry(frame.func_index())
                    .or_insert_with(|| match frame.func_name() {
                        Some(name) => name.into(),
                        None => format!("func[{}]", frame.func_index()).into(),
                    })
                    .clone()
            } else {
                self.guest_code.clone()
            };
            self.stack.push(name);
        }

        self.charge(in_wasm);
        for (i, ns) in [idle, compile, jit_other, display, other].into_iter().enumerate() {
            if ns > 0 {
                self.stack.push(self.host_frames[i].clone());
                self.charge(ns);
                self.stack.pop();
            }
        }
        if outside > 0 {
            let stack = std::mem::replace(&mut self.stack, vec![self.outside.clone()]);
            self.charge(outside);
            self.stack = stack;
        }

        if now - self.window_start >= self.settings.every {
            self.report();
        }
    }

    fn charge(&mut self, ns: u64) {
        if ns == 0 {
            return;
        }
        let w = &mut self.window;
        w.total_ns += ns;
        if let Some(top) = self.stack.last() {
            *w.self_ns.entry(top.clone()).or_default() += ns;
        }
        self.seen.clear();
        for f in &self.stack {
            if !self.seen.iter().any(|s| Arc::ptr_eq(s, f)) {
                self.seen.push(f.clone());
                *w.total_ns_by.entry(f.clone()).or_default() += ns;
            }
        }
        if self.settings.dir.is_some() {
            let key = self.stack.iter().map(|f| &**f).collect::<Vec<_>>().join(";");
            *w.folded.entry(key).or_default() += ns;
        }
    }

    /// Print the window's results (and write its stacks), then start a new one.
    fn report(&mut self) {
        let w = std::mem::take(&mut self.window);
        let now = Instant::now();
        let from = (self.window_start - self.started).as_secs_f64();
        let to = (now - self.started).as_secs_f64();
        self.window_start = now;
        self.window_number += 1;
        if w.total_ns == 0 {
            return;
        }
        let pct = |ns: u64| 100.0 * ns as f64 / w.total_ns as f64;
        let self_of = |name: &Arc<str>| w.self_ns.get(name).copied().unwrap_or(0);
        let host: u64 = self.host_frames.iter().map(|f| self_of(f)).sum();
        let guest = self_of(&self.guest_code);
        let outside = self_of(&self.outside);
        let mut out = String::new();
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "[qemu-profile] window {} ({from:.0}-{to:.0} s): {} samples, {:.0} host calls/s",
            self.window_number,
            w.samples,
            w.host_calls as f64 / (to - from).max(1e-9)
        );
        let _ = writeln!(
            out,
            "[qemu-profile]   guest code {:.1}%, QEMU's own code {:.1}%, host calls {:.1}% \
             (idle {:.1}, JIT compile {:.1}, JIT instantiate {:.1}, display {:.1}, other {:.1}), \
             outside QEMU {:.1}%",
            pct(guest),
            pct(w.total_ns.saturating_sub(host + guest + outside)),
            pct(host),
            pct(self_of(&self.host_frames[0])),
            pct(self_of(&self.host_frames[1])),
            pct(self_of(&self.host_frames[2])),
            pct(self_of(&self.host_frames[3])),
            pct(self_of(&self.host_frames[4])),
            pct(outside),
        );
        let mut by_self: Vec<(&Arc<str>, &u64)> = w.self_ns.iter().collect();
        by_self.sort_by(|a, b| b.1.cmp(a.1));
        let _ = writeln!(out, "[qemu-profile]    self%  total%  function (by self time)");
        for (name, ns) in by_self.iter().take(self.settings.top) {
            let total = w.total_ns_by.get(*name).copied().unwrap_or(0);
            let _ = writeln!(out, "[qemu-profile]   {:6.2}  {:6.2}  {name}", pct(**ns), pct(total));
        }
        let mut by_total: Vec<(&Arc<str>, &u64)> = w.total_ns_by.iter().collect();
        by_total.sort_by(|a, b| b.1.cmp(a.1));
        let _ = writeln!(out, "[qemu-profile]    self%  total%  function (by total time)");
        for (name, ns) in by_total.iter().take(self.settings.top) {
            let _ = writeln!(out, "[qemu-profile]   {:6.2}  {:6.2}  {name}", pct(self_of(name)), pct(**ns));
        }
        eprint!("{out}");

        if let Some(dir) = &self.settings.dir {
            let path = dir.join(format!("window-{:03}.folded", self.window_number));
            let result = std::fs::File::create(&path).and_then(|mut f| {
                for (stack, ns) in &w.folded {
                    // Flame graph tools want integer weights: microseconds.
                    writeln!(f, "{stack} {}", ns / 1000)?;
                }
                Ok(())
            });
            if let Err(e) = result {
                eprintln!("[qemu-profile] cannot write {}: {e}", path.display());
            }
        }
    }
}
