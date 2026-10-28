/// QEMU JIT Host Extension for m-browser
///
/// This module provides:
///   1. `QemuJitState` — holds compiled translation block (TB) modules
///   2. `add_jit_to_linker()` — registers `wasi_jit_compile`, `wasi_jit_call`,
///      `wasi_jit_drop` as host-provided imports in a core `wasmtime::Linker`
///   3. `QemuHostState` — the Store data type for running QEMU as a core module
///      with WASIp1 + JIT support
///
/// The QEMU wasm binary imports these three functions from the `env` module
/// (they land there because they were declared as plain C externs with
/// `--allow-undefined` during linking).
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use wasmtime::{
    AsContextMut, Caller, Engine, Extern, Func, FuncType, Instance, Linker, Memory, Module, Table,
    TypedFunc, Val,
};

/// Maps keyed by a TB module's wasm bytes. FxHash is several times faster
/// than the default SipHash on these 1-4 KB keys, which are looked up on
/// every load (`jit_load`), not just on compiles.
type BytesMap<V> = HashMap<Arc<[u8]>, V, rustc_hash::FxBuildHasher>;
type BytesSet = HashSet<Arc<[u8]>, rustc_hash::FxBuildHasher>;

// ---------------------------------------------------------------------------
//  Compiled Translation Block
// ---------------------------------------------------------------------------

struct CompiledTB {
    #[allow(dead_code)]
    instance: Instance,
    start_func: TypedFunc<(i32,), (i32,)>,
}

// ---------------------------------------------------------------------------
//  JIT State
// ---------------------------------------------------------------------------

/// The compiled TB modules, and the compiler threads that add to them:
/// shared by every Store QEMU runs in (each vCPU thread has its own in the
/// multi-threaded build, see qemu_threads.rs).
pub struct JitCache {
    engine: Engine,
    /// Compiled TB modules keyed by their wasm bytes. A `Module` belongs to
    /// the Engine, not a Store, so after a Store migration a TB whose code is
    /// unchanged only needs re-instantiating, not recompiling. Bounded FIFO:
    /// every live Module holds executable memory mappings.
    module_cache: BytesMap<Module>,
    module_cache_order: VecDeque<Arc<[u8]>>,
    /// Compiles TB modules on other threads for wasi_jit_load_async.
    compiler: Option<BackgroundCompiler>,
    /// Statistics, reported at each Store migration.
    pub stats_compiled: u64,
    pub stats_cache_hits: u64,
    pub stats_compile_time: std::time::Duration,
    /// Compile count and time by module size: <512, <1K, <2K, <4K, >=4K bytes.
    pub stats_by_size: [(u64, std::time::Duration, u64); 5],
}

/// The TB modules instantiated in one Store.
pub struct QemuJitState {
    pub cache: Arc<Mutex<JitCache>>,
    modules: HashMap<u32, CompiledTB>,
    /// The table slot holding the instance of each module in the current
    /// Store. TB modules have no state of their own (TCG's registers are
    /// locals) and their code is position independent, so every TB with the
    /// same code can run the same instance: retranslating guest code (after
    /// a TB buffer flush, say) costs a table lookup instead of a new
    /// instance, and instances, which only a Store migration frees, pile up
    /// far more slowly.
    by_key: BytesMap<u32>,
    /// TB module instances created in the current Store. Wasmtime frees
    /// instances only when their Store is dropped, so this grows until the
    /// runtime moves QEMU into a fresh Store (see `wasm::migrate_store`).
    pub instances_in_store: u32,
    /// Loads served by an instance already in the Store.
    pub stats_shared: u64,
}

/// Compiles TB modules on worker threads, so that QEMU's thread does not
/// stop for each one (Cranelift takes about a millisecond per TB); QEMU
/// keeps interpreting a TB until its module is ready (wasi_jit_load_async).
struct BackgroundCompiler {
    jobs: std::sync::mpsc::Sender<Arc<[u8]>>,
    /// Finished compiles, collected by QEMU's thread (`collect`).
    done: Arc<Mutex<Vec<(Arc<[u8]>, Option<Module>)>>>,
    /// Queued or being compiled.
    pending: BytesSet,
    /// Modules the compiler rejected; QEMU keeps those TBs interpreted.
    failed: BytesSet,
    compiled: Arc<AtomicU64>,
    compile_ns: Arc<AtomicU64>,
}

impl BackgroundCompiler {
    /// Start `threads` compiler threads (QEMU_JIT_THREADS, default: a
    /// quarter of the host's CPUs, 1 to 4).
    fn start(engine: &Engine) -> Self {
        let threads = std::env::var("QEMU_JIT_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or_else(|| {
                let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
                (cpus / 4).clamp(1, 4)
            })
            .max(1);
        let (jobs, rx) = std::sync::mpsc::channel::<Arc<[u8]>>();
        let rx = Arc::new(Mutex::new(rx));
        let done = Arc::new(Mutex::new(Vec::new()));
        let compiled = Arc::new(AtomicU64::new(0));
        let compile_ns = Arc::new(AtomicU64::new(0));
        for i in 0..threads {
            let (engine, rx, done) = (engine.clone(), rx.clone(), done.clone());
            let (compiled, compile_ns) = (compiled.clone(), compile_ns.clone());
            std::thread::Builder::new()
                .name(format!("qemu-jit-{i}"))
                .spawn(move || loop {
                    // The lock is only held while waiting for the next job.
                    let job = rx.lock().ok().and_then(|rx| rx.recv().ok());
                    let Some(wasm) = job else { return };
                    let started = Instant::now();
                    let module = Module::new(&engine, &wasm[..]);
                    compile_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    compiled.fetch_add(1, Ordering::Relaxed);
                    if let Err(e) = &module {
                        eprintln!("[qemu-jit] background compile failed: {e:#}");
                    }
                    if let Ok(mut done) = done.lock() {
                        done.push((wasm, module.ok()));
                    }
                })
                .expect("cannot start a JIT compiler thread");
        }
        eprintln!("[qemu-jit] compiling TBs in the background on {threads} threads");
        Self {
            jobs,
            done,
            pending: BytesSet::default(),
            failed: BytesSet::default(),
            compiled,
            compile_ns,
        }
    }
}

/// Maximum number of compiled TB modules kept for reuse.
/// Each cached module holds about two memory mappings, which count against
/// vm.max_map_count (usually 65,530) together with modules still used by
/// the current Store (up to MIGRATE_AFTER_INSTANCES).
const MODULE_CACHE_MAX: usize = 18_000;

impl JitCache {
    pub fn new(engine: &Engine) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            engine: engine.clone(),
            module_cache: BytesMap::default(),
            module_cache_order: VecDeque::new(),
            compiler: None,
            stats_compiled: 0,
            stats_cache_hits: 0,
            stats_compile_time: std::time::Duration::ZERO,
            stats_by_size: [(0, std::time::Duration::ZERO, 0); 5],
        }))
    }

    /// Drop the oldest cached modules, keeping at most `keep`. Used when the
    /// cache alone holds too many memory mappings.
    pub fn shrink_module_cache(&mut self, keep: usize) {
        while self.module_cache.len() > keep {
            match self.module_cache_order.pop_front() {
                Some(old) => {
                    self.module_cache.remove(&old);
                }
                None => break,
            }
        }
    }

    pub fn module_cache_len(&self) -> usize {
        self.module_cache.len()
    }

    /// Add a compiled module to the bounded module cache.
    fn cache_module(&mut self, key: Arc<[u8]>, module: Module) {
        if self.module_cache.len() >= MODULE_CACHE_MAX {
            if let Some(old) = self.module_cache_order.pop_front() {
                self.module_cache.remove(&old);
            }
        }
        if self.module_cache.insert(key.clone(), module).is_none() {
            self.module_cache_order.push_back(key);
        }
    }

    /// Return the compiled module for `wasm`, compiling it on a cache miss.
    fn get_or_compile(&mut self, wasm: &[u8]) -> Result<Module> {
        if let Some(m) = self.module_cache.get(wasm) {
            self.stats_cache_hits += 1;
            return Ok(m.clone());
        }
        let started = std::time::Instant::now();
        let module = Module::new(&self.engine, wasm)?;
        let took = started.elapsed();
        crate::qemu_profile::add_host_work(crate::qemu_profile::HostWork::JitCompile, took);
        self.stats_compile_time += took;
        self.stats_compiled += 1;
        let bucket = match wasm.len() {
            0..=511 => 0,
            512..=1023 => 1,
            1024..=2047 => 2,
            2048..=4095 => 3,
            _ => 4,
        };
        let entry = &mut self.stats_by_size[bucket];
        entry.0 += 1;
        entry.1 += took;
        entry.2 += wasm.len() as u64;
        dump_tb_module(self.stats_compiled, wasm);

        self.cache_module(wasm.into(), module.clone());
        Ok(module)
    }

    /// Move finished background compiles into the module cache.
    fn collect_background(&mut self) {
        let Some(compiler) = &mut self.compiler else { return };
        let finished = match compiler.done.lock() {
            Ok(mut done) if !done.is_empty() => std::mem::take(&mut *done),
            _ => return,
        };
        let mut ready = Vec::with_capacity(finished.len());
        for (key, module) in finished {
            compiler.pending.remove(&key);
            match module {
                Some(module) => ready.push((key, module)),
                None => {
                    compiler.failed.insert(key);
                }
            }
        }
        for (key, module) in ready {
            dump_tb_module(self.stats_compiled + self.background_compiled(), &key);
            self.cache_module(key, module);
        }
    }

    /// Queue `key` for compiling in the background, unless it is already.
    fn compile_in_background(&mut self, key: &[u8]) {
        let engine = self.engine.clone();
        let compiler = self.compiler.get_or_insert_with(|| BackgroundCompiler::start(&engine));
        if compiler.pending.contains(key) {
            return;
        }
        let key: Arc<[u8]> = key.into();
        if compiler.jobs.send(key.clone()).is_ok() {
            compiler.pending.insert(key);
        }
    }

    /// TB modules compiled in the background so far, and the time it took.
    pub fn background_compiled(&self) -> u64 {
        self.compiler.as_ref().map_or(0, |c| c.compiled.load(Ordering::Relaxed))
    }

    pub fn background_compile_time(&self) -> Duration {
        self.compiler
            .as_ref()
            .map_or(Duration::ZERO, |c| Duration::from_nanos(c.compile_ns.load(Ordering::Relaxed)))
    }

}

impl QemuJitState {
    pub fn new(cache: Arc<Mutex<JitCache>>) -> Self {
        Self {
            cache,
            modules: HashMap::new(),
            by_key: BytesMap::default(),
            instances_in_store: 0,
            stats_shared: 0,
        }
    }

    /// Forget every compiled TB because the Store holding their instances is
    /// being replaced. The new QEMU instance starts with a fresh function
    /// table, so no slots are in use any more either.
    pub fn reset_for_new_store(&mut self) {
        self.modules.clear();
        self.by_key.clear();
        self.instances_in_store = 0;
    }
}

/// QEMU_JIT_DUMP=<dir>[,<every>]: save every <every>th (default 100th)
/// translation block module compiled to <dir>/tb-<n>.wasm, to study the code
/// QEMU generates (e.g. with `wasm-tools print`).
fn dump_tb_module(n: u64, wasm: &[u8]) {
    static SETTINGS: std::sync::OnceLock<Option<(std::path::PathBuf, u64)>> = std::sync::OnceLock::new();
    let settings = SETTINGS.get_or_init(|| {
        let spec = std::env::var("QEMU_JIT_DUMP").ok()?;
        let (dir, every) = match spec.split_once(',') {
            Some((dir, every)) => (dir.to_string(), every.parse().unwrap_or(100)),
            None => (spec, 100),
        };
        std::fs::create_dir_all(&dir).ok()?;
        Some((dir.into(), every.max(1)))
    });
    if let Some((dir, every)) = settings {
        if n % every == 0 {
            let _ = std::fs::write(dir.join(format!("tb-{n:07}.wasm")), wasm);
        }
    }
}

// ---------------------------------------------------------------------------
//  Host state for the QEMU core-module Store
// ---------------------------------------------------------------------------

use crate::qemu_gfx::QemuGfxState;
use wasmtime_wasi::p1::WasiP1Ctx;

/// QEMU's linear memory: its own, or, in the multi-threaded build, imported
/// and shared by all of its threads.
#[derive(Clone)]
pub enum GuestMem {
    Plain(Memory),
    Shared(wasmtime::SharedMemory),
}

impl GuestMem {
    pub fn to_extern(&self) -> Extern {
        match self {
            GuestMem::Plain(m) => Extern::Memory(*m),
            GuestMem::Shared(m) => Extern::SharedMemory(m.clone()),
        }
    }
}

/// Run `f` on the `len` bytes of QEMU's memory at `ptr`; `None` if they are
/// out of bounds. Other threads may be writing shared memory meanwhile: `f`
/// must not rely on the bytes staying the same.
pub fn with_guest<R>(
    caller: &Caller<'_, QemuHostState>,
    ptr: usize,
    len: usize,
    f: impl FnOnce(&[u8]) -> R,
) -> Option<R> {
    let end = ptr.checked_add(len)?;
    match caller.data().guest_memory.as_ref()? {
        GuestMem::Plain(m) => m.data(caller).get(ptr..end).map(f),
        GuestMem::Shared(m) => {
            let cells = m.data().get(ptr..end)?;
            // SAFETY: in bounds; racing writes from other threads only make
            // the snapshot inconsistent, which callers tolerate.
            let bytes = unsafe { std::slice::from_raw_parts(cells.as_ptr() as *const u8, len) };
            Some(f(bytes))
        }
    }
}

/// Like `with_guest`, to write QEMU's memory.
pub fn with_guest_mut<R>(
    caller: &mut Caller<'_, QemuHostState>,
    ptr: usize,
    len: usize,
    f: impl FnOnce(&mut [u8]) -> R,
) -> Option<R> {
    let end = ptr.checked_add(len)?;
    match caller.data().guest_memory.clone()? {
        GuestMem::Plain(m) => m.data_mut(caller).get_mut(ptr..end).map(f),
        GuestMem::Shared(m) => {
            let cells = m.data().get(ptr..end)?;
            // SAFETY: in bounds; QEMU hands the host buffers that no other
            // thread uses meanwhile.
            let bytes =
                unsafe { std::slice::from_raw_parts_mut(cells.as_ptr() as *mut u8, len) };
            Some(f(bytes))
        }
    }
}

/// The WASI context of a Store: its own, or the one all of QEMU's threads
/// share (see `qemu_threads::SharedWasi`).
pub enum WasiSlot {
    Own(WasiP1Ctx),
    Shared(Arc<crate::qemu_threads::SharedWasi>),
}

pub struct QemuHostState {
    pub wasi: WasiSlot,
    /// Whether this Store's thread holds the shared WASI context's lock,
    /// which it does from a WASI call until it returns (see `wasi_ctx`).
    pub wasi_locked: bool,
    pub jit: QemuJitState,
    pub gfx: Arc<Mutex<QemuGfxState>>,
    /// Cached reference to the guest's linear memory (set after instantiation)
    pub guest_memory: Option<GuestMem>,
    /// Cached reference to QEMU's function table, which TB modules import and
    /// their entry functions go into (set after instantiation)
    pub guest_table: Option<Table>,
    /// Set to true when a TB module's helper trampoline catches a WasmException
    /// (typically a wasm `throw` from siglongjmp inside a helper). The C wrapper
    /// `tcg_qemu_tb_exec` polls this via `wasi_jit_take_pending_exception` and
    /// turns it into a proper `cpu_loop_exit` so the longjmp chain works.
    pub pending_exception: bool,
    /// Store resource limits. Raises wasmtime's default cap of 10,000
    /// instances per Store, which a burst of new TBs between two Store
    /// migrations could otherwise exceed.
    pub limits: wasmtime::StoreLimits,
}

impl QemuHostState {
    /// The WASI context, for wasmtime-wasi's functions. With a shared one,
    /// this takes its lock, which a call hook on the Store releases when the
    /// WASI function returns (`qemu_threads::install_store_hooks`).
    pub fn wasi_ctx(&mut self) -> &mut WasiP1Ctx {
        match &mut self.wasi {
            WasiSlot::Own(ctx) => ctx,
            WasiSlot::Shared(shared) => {
                if !self.wasi_locked {
                    shared.lock();
                    self.wasi_locked = true;
                }
                // SAFETY: this thread holds the lock (see SharedWasi).
                unsafe { &mut *shared.ctx.get() }
            }
        }
    }

    /// Release the shared WASI context's lock, if this Store holds it.
    pub fn release_wasi(&mut self) {
        if self.wasi_locked {
            self.wasi_locked = false;
            if let WasiSlot::Shared(shared) = &self.wasi {
                shared.unlock();
            }
        }
    }
}

/// Store limits for the QEMU Store (see `QemuHostState::limits`). Above the
/// instance count at which the runtime moves QEMU to a fresh Store
/// (`MIGRATE_AFTER_INSTANCES` in wasm.rs), with room for one more step.
pub fn qemu_store_limits() -> wasmtime::StoreLimits {
    wasmtime::StoreLimitsBuilder::new().instances(150_000).build()
}

// ---------------------------------------------------------------------------
//  Add JIT imports to a core-module Linker<QemuHostState>
// ---------------------------------------------------------------------------

pub fn add_jit_to_linker(linker: &mut Linker<QemuHostState>) -> Result<()> {
    // -----------------------------------------------------------------------
    //  env::wasi_jit_compile
    //
    //  Signature (from the C side):
    //    uint32_t wasi_jit_compile(
    //        const uint8_t *wasm_bytes,     // i32 ptr
    //        size_t wasm_bytes_len,          // i32 len
    //        const uint32_t *helper_indices, // i32 ptr
    //        size_t helper_func_count        // i32 count
    //    ) -> uint32_t handle (0 = failure)
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "wasi_jit_compile",
        |mut caller: Caller<'_, QemuHostState>,
         wasm_ptr: i32,
         wasm_len: i32,
         helpers_ptr: i32,
         helpers_len: i32|
         -> u32 { jit_load(&mut caller, wasm_ptr, wasm_len, helpers_ptr, helpers_len, LoadMode::Compile) },
    )?;

    // -----------------------------------------------------------------------
    //  env::wasi_jit_load_cached
    //
    //  Same as wasi_jit_compile, but only loads a module that is already in
    //  the module cache, and returns 0 instead of compiling. QEMU uses it to
    //  put TBs that were hot before (e.g. ahead of a TB buffer flush) straight
    //  back on the JIT tier.
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "wasi_jit_load_cached",
        |mut caller: Caller<'_, QemuHostState>,
         wasm_ptr: i32,
         wasm_len: i32,
         helpers_ptr: i32,
         helpers_len: i32|
         -> u32 { jit_load(&mut caller, wasm_ptr, wasm_len, helpers_ptr, helpers_len, LoadMode::CachedOnly) },
    )?;

    // -----------------------------------------------------------------------
    //  env::wasi_jit_load_async
    //
    //  Same as wasi_jit_compile when the module is compiled already.
    //  Otherwise it is queued for compiling on another thread and 0 is
    //  returned: QEMU keeps interpreting the TB and asks again later. Returns
    //  JIT_LOAD_FAILED (all ones) once the compiler has rejected the module.
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "wasi_jit_load_async",
        |mut caller: Caller<'_, QemuHostState>,
         wasm_ptr: i32,
         wasm_len: i32,
         helpers_ptr: i32,
         helpers_len: i32|
         -> u32 { jit_load(&mut caller, wasm_ptr, wasm_len, helpers_ptr, helpers_len, LoadMode::Async) },
    )?;

    // -----------------------------------------------------------------------
    //  env::wasi_jit_call
    //
    //  uint32_t wasi_jit_call(uint32_t module_handle, uint32_t ctx_ptr)
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "wasi_jit_call",
        |mut caller: Caller<'_, QemuHostState>, handle: u32, ctx_ptr: u32| -> u32 {
            let start_func = match caller.data().jit.modules.get(&handle) {
                Some(tb) => tb.start_func.clone(),
                None => {
                    eprintln!("[qemu-jit] call: unknown handle {handle}");
                    // Clear ctx.tb_ptr so the guest's TB loop stops instead of
                    // retrying the same unknown handle forever (same as the
                    // execution-error path below).
                    with_guest_mut(&mut caller, ctx_ptr as usize + 8, 4, |tb_ptr| {
                        tb_ptr.copy_from_slice(&0u32.to_le_bytes())
                    });
                    return 0;
                }
            };

            match start_func.call(&mut caller, (ctx_ptr as i32,)) {
                Ok((result,)) => result as u32,
                Err(e) => {
                    // Log full error chain only the first time we see this handle fail.
                    use std::collections::HashSet;
                    use std::sync::Mutex;
                    static SEEN: Mutex<Option<HashSet<u32>>> = Mutex::new(None);
                    let first = {
                        let mut g = SEEN.lock().unwrap();
                        if g.is_none() {
                            *g = Some(HashSet::new());
                        }
                        g.as_mut().unwrap().insert(handle)
                    };
                    if first {
                        eprintln!("[qemu-jit] call: execution error handle={handle}: {e}");
                        eprintln!("[qemu-jit]   debug: {e:#?}");
                        // Print source chain
                        let mut src: &dyn std::error::Error = e.as_ref();
                        let mut depth = 0;
                        while let Some(s) = src.source() {
                            eprintln!("[qemu-jit]   caused by [{depth}]: {s}");
                            src = s;
                            depth += 1;
                            if depth > 10 {
                                break;
                            }
                        }
                        // Print wasmtime trap details if any
                        if let Some(trap) = e.downcast_ref::<wasmtime::Trap>() {
                            eprintln!("[qemu-jit]   trap: {trap:?}");
                        }
                        // Print wasm backtrace if available
                        if let Some(bt) = e.downcast_ref::<wasmtime::WasmBacktrace>() {
                            eprintln!("[qemu-jit]   wasm backtrace:\n{bt}");
                        }
                    }

                    // CRITICAL: clear ctx.tb_ptr so the C-side loop in tcg_qemu_tb_exec exits.
                    // Otherwise it retries the same failing TB forever.
                    // wasmContext layout: env(0) stack(4) tb_ptr(8) ...
                    with_guest_mut(&mut caller, ctx_ptr as usize + 8, 4, |tb_ptr| {
                        tb_ptr.copy_from_slice(&0u32.to_le_bytes())
                    });
                    0
                }
            }
        },
    )?;

    // -----------------------------------------------------------------------
    //  env::wasi_jit_drop
    //
    //  void wasi_jit_drop(uint32_t module_handle)
    //
    //  QEMU no longer uses this handle for a TB. The instance stays, and so
    //  does its table slot: TBs with the same code share both (`by_key`), so
    //  a retranslation of this code can have them back for a lookup, and
    //  only a Store migration (which also resets the table) frees instances.
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "wasi_jit_drop",
        |caller: Caller<'_, QemuHostState>, handle: u32| {
            if !caller.data().jit.modules.contains_key(&handle) {
                eprintln!("[qemu-jit] drop: unknown handle {handle}");
            }
        },
    )?;

    // -----------------------------------------------------------------------
    //  env::wasi_jit_take_pending_exception
    //
    //  int wasi_jit_take_pending_exception(void)
    //
    //  Returns 1 (and clears the flag) if a TB helper call most recently
    //  threw a wasm exception (e.g., from siglongjmp inside a QEMU helper).
    //  The C wrapper around tcg_qemu_tb_exec uses this to translate the
    //  swallowed wasm-exception into a proper cpu_loop_exit() on the host
    //  module side, which CAN siglongjmp back to cpu_exec's sigsetjmp.
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "wasi_jit_take_pending_exception",
        |mut caller: Caller<'_, QemuHostState>| -> i32 {
            let st = caller.data_mut();
            if st.pending_exception {
                st.pending_exception = false;
                1
            } else {
                0
            }
        },
    )?;

    Ok(())
}

/// How `jit_load` gets a module that is not compiled yet.
#[derive(Clone, Copy, PartialEq)]
enum LoadMode {
    /// Compile it now (wasi_jit_compile).
    Compile,
    /// Don't: return 0 (wasi_jit_load_cached).
    CachedOnly,
    /// Compile it on another thread and return 0 for now, or
    /// `JIT_LOAD_FAILED` once that has failed (wasi_jit_load_async).
    Async,
}

/// wasi_jit_load_async's answer for a module the compiler rejected.
const JIT_LOAD_FAILED: u32 = u32::MAX;

/// Load the TB module at `wasm_ptr` into the Store (see `wasi_jit_compile`)
/// and return its handle, or 0 if it is not available (see `LoadMode`).
fn jit_load(
    caller: &mut Caller<'_, QemuHostState>,
    wasm_ptr: i32,
    wasm_len: i32,
    helpers_ptr: i32,
    helpers_len: i32,
    mode: LoadMode,
) -> u32 {
    let _timer = crate::qemu_profile::time_host(crate::qemu_profile::HostWork::JitTotal);
    let Some(memory) = caller.data().guest_memory.clone() else {
        eprintln!("[qemu-jit] compile: guest memory not set");
        return 0;
    };

    // An instance of this very module in the Store already serves any TB
    // with the same code; otherwise copy the bytes out.
    let ws = wasm_ptr as u32 as usize;
    let len = wasm_len as u32 as usize;
    let found = with_guest(caller, ws, len, |wasm| match caller.data().jit.by_key.get(wasm) {
        Some(&handle) => (Some(handle), None),
        None => (None, Some(Arc::<[u8]>::from(wasm))),
    });
    let wasm_bytes = match found {
        None => {
            eprintln!("[qemu-jit] compile: wasm bytes out of bounds ({ws}+{len})");
            return 0;
        }
        Some((Some(handle), _)) => {
            caller.data_mut().jit.stats_shared += 1;
            return handle;
        }
        Some((None, bytes)) => bytes.expect("set when not shared"),
    };

    // Compile the TB wasm module, or reuse an identical one compiled
    // before (by an earlier Store, or in the background).
    let cache = caller.data().jit.cache.clone();
    let mut guard = cache.lock().unwrap();
    let jit = &mut *guard;
    jit.collect_background();
    let module = match mode {
        LoadMode::Compile => match jit.get_or_compile(&wasm_bytes) {
            Ok(m) => m,
            Err(e) => {
                // Keep the failing module for offline inspection
                // (e.g. `wasm-tools validate` / `wasm-tools print`).
                let path = std::env::temp_dir().join("qemu-jit-failed.wasm");
                let _ = std::fs::write(&path, &wasm_bytes);
                eprintln!(
                    "[qemu-jit] compile: module compilation failed: {e:#} (module saved to {})",
                    path.display()
                );
                return 0;
            }
        },
        LoadMode::CachedOnly | LoadMode::Async => match jit.module_cache.get(&wasm_bytes[..]) {
            Some(m) => {
                jit.stats_cache_hits += 1;
                m.clone()
            }
            None if mode == LoadMode::CachedOnly => return 0,
            None => {
                if jit.compiler.as_ref().is_some_and(|c| c.failed.contains(&wasm_bytes[..])) {
                    return JIT_LOAD_FAILED;
                }
                jit.compile_in_background(&wasm_bytes);
                return 0;
            }
        },
    };
    drop(guard);

    // Resolve the TB module's imports:
    //   env.buffer : QEMU's linear memory
    //   env.table  : QEMU's function table. Current QEMU builds call
    //                helpers with call_indirect through it, so helper
    //                calls never leave wasm.
    //   helper.N   : per-helper host trampolines (older QEMU builds)
    let guest_table = match caller.data().guest_table {
        Some(t) => t,
        None => match caller.get_export("__indirect_function_table") {
            Some(Extern::Table(t)) => t,
            _ => {
                eprintln!("[qemu-jit] compile: __indirect_function_table not found");
                return 0;
            }
        },
    };
    let direct_imports: Option<Vec<Extern>> = module
        .imports()
        .map(|imp| match (imp.module(), imp.name()) {
            ("env", "buffer") => Some(memory.to_extern()),
            ("env", "table") => Some(Extern::Table(guest_table)),
            _ => None,
        })
        .collect();
    let instantiated = match direct_imports {
        // The usual case: pass the imports directly, no Linker needed.
        Some(imports) => Instance::new(caller.as_context_mut(), &module, &imports),
        None => {
            let helper_indices = match read_helper_indices(caller, helpers_ptr, helpers_len) {
                Some(h) => h,
                None => return 0,
            };
            match legacy_linker(caller, &module, memory.to_extern(), guest_table, &helper_indices) {
                Some(tb_linker) => tb_linker.instantiate(caller.as_context_mut(), &module),
                None => return 0,
            }
        }
    };

    // Instantiate the TB module
    let instance = match instantiated {
        Ok(inst) => inst,
        Err(e) => {
            eprintln!("[qemu-jit] compile: instantiation failed: {e}");
            return 0;
        }
    };
    let Some(start) = instance.get_func(caller.as_context_mut(), "start") else {
        eprintln!("[qemu-jit] compile: 'start' export not found");
        return 0;
    };
    let start_func = match start.typed::<(i32,), (i32,)>(&*caller) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("[qemu-jit] compile: 'start' has the wrong type: {e}");
            return 0;
        }
    };

    // Put the entry function into QEMU's function table. Its index is
    // the handle: QEMU calls the TB through it as a C function
    // pointer, a direct wasm-to-wasm call_indirect.
    let entry = wasmtime::Ref::Func(Some(start));
    let handle = match guest_table.grow(caller.as_context_mut(), 1, entry) {
        Ok(index) => index as u32,
        Err(e) => {
            eprintln!("[qemu-jit] compile: table grow failed: {e}");
            return 0;
        }
    };

    let jit = &mut caller.data_mut().jit;
    jit.instances_in_store += 1;
    jit.modules.insert(handle, CompiledTB { instance, start_func });
    jit.by_key.insert(wasm_bytes, handle);
    handle
}

/// Read QEMU's array of helper function table indices (u32 LE), which older
/// QEMU builds' TB modules need (see `legacy_linker`).
fn read_helper_indices(
    caller: &Caller<'_, QemuHostState>,
    helpers_ptr: i32,
    helpers_len: i32,
) -> Option<Vec<u32>> {
    let hs = helpers_ptr as u32 as usize;
    let len = (helpers_len as u32 as usize) * 4;
    let indices = with_guest(caller, hs, len, |data| {
        data.chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect()
    });
    if indices.is_none() {
        eprintln!("[qemu-jit] compile: helper indices out of bounds ({hs}+{len})");
    }
    indices
}

/// A Linker for TB modules of older QEMU builds, which import each helper
/// as `helper.N` (a host trampoline into QEMU's function table) instead of
/// calling it through the table themselves.
fn legacy_linker(
    caller: &mut Caller<'_, QemuHostState>,
    module: &Module,
    memory: Extern,
    guest_table: Table,
    helper_indices: &[u32],
) -> Option<Linker<QemuHostState>> {
    let engine = caller.engine().clone();
    let mut tb_linker: Linker<QemuHostState> = Linker::new(&engine);
    if let Err(e) = tb_linker.define(&*caller, "env", "buffer", memory) {
        eprintln!("[qemu-jit] compile: define env/buffer failed: {e}");
        return None;
    }
    if let Err(e) = tb_linker.define(&*caller, "env", "table", guest_table) {
        eprintln!("[qemu-jit] compile: define env/table failed: {e}");
        return None;
    }
    let legacy_helpers: Vec<(String, FuncType)> = module
        .imports()
        .filter(|imp| imp.module() == "helper")
        .filter_map(|imp| Some((imp.name().to_string(), imp.ty().func()?.clone())))
        .collect();
    for (name, func_type) in legacy_helpers {
        let Some(&table_index) = name.parse::<usize>().ok().and_then(|i| helper_indices.get(i))
        else {
            eprintln!("[qemu-jit] compile: no table index for helper/{name}");
            return None;
        };
        let table = guest_table;
        let trampoline = Func::new(
            caller.as_context_mut(),
            func_type,
            move |mut caller: Caller<'_, QemuHostState>,
                  params: &[Val],
                  results: &mut [Val]|
                  -> Result<()> {
                let func = match table
                    .get(&mut caller, table_index as u64)
                    .context("helper table index OOB")?
                {
                    wasmtime::Ref::Func(Some(f)) => f,
                    _ => anyhow::bail!("table entry {table_index} is not a function"),
                };
                let mut call_results = vec![Val::I32(0); results.len()];
                match func.call(&mut caller, params, &mut call_results) {
                    Ok(()) => {
                        results.clone_from_slice(&call_results);
                        Ok(())
                    }
                    Err(e) => {
                        // A helper threw (siglongjmp). Wasm exceptions
                        // cannot cross this host frame, so QEMU re-raises
                        // it via wasi_jit_take_pending_exception.
                        caller.data_mut().pending_exception = true;
                        Err(e)
                    }
                }
            },
        );
        if let Err(e) = tb_linker.define(&*caller, "helper", &name, trampoline) {
            eprintln!("[qemu-jit] compile: define helper/{name} failed: {e}");
            return None;
        }
    }
    Some(tb_linker)
}
