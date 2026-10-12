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
use std::collections::HashMap;
use wasmtime::{
    AsContextMut, Caller, Engine, Func, FuncType, Instance, Linker, Memory, Module, Store,
    TypedFunc, Val, ValType,
};

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

pub struct QemuJitState {
    engine: Engine,
    modules: HashMap<u32, CompiledTB>,
    /// Recycled slots in QEMU's function table (see wasi_jit_drop).
    free_slots: Vec<u32>,
    /// TB module instances created in the current Store. Wasmtime frees
    /// instances only when their Store is dropped, so this grows until the
    /// runtime moves QEMU into a fresh Store (see `wasm::migrate_store`).
    pub instances_in_store: u32,
    /// Compiled TB modules keyed by their wasm bytes. A `Module` belongs to
    /// the Engine, not a Store, so after a Store migration a TB whose code is
    /// unchanged only needs re-instantiating, not recompiling. Bounded FIFO:
    /// every live Module holds executable memory mappings.
    module_cache: HashMap<std::sync::Arc<[u8]>, Module>,
    module_cache_order: std::collections::VecDeque<std::sync::Arc<[u8]>>,
    /// Statistics, reported at each Store migration.
    pub stats_compiled: u64,
    pub stats_cache_hits: u64,
    pub stats_compile_time: std::time::Duration,
    /// Compile count and time by module size: <512, <1K, <2K, <4K, >=4K bytes.
    pub stats_by_size: [(u64, std::time::Duration, u64); 5],
}

/// Maximum number of compiled TB modules kept for reuse.
/// Each cached module holds about two memory mappings, which count against
/// vm.max_map_count (usually 65,530) together with modules still used by
/// the current Store (up to MIGRATE_AFTER_INSTANCES).
const MODULE_CACHE_MAX: usize = 18_000;

impl QemuJitState {
    pub fn new(engine: &Engine) -> Self {
        Self {
            engine: engine.clone(),
            modules: HashMap::new(),
            free_slots: Vec::new(),
            instances_in_store: 0,
            module_cache: HashMap::new(),
            module_cache_order: std::collections::VecDeque::new(),
            stats_compiled: 0,
            stats_cache_hits: 0,
            stats_compile_time: std::time::Duration::ZERO,
            stats_by_size: [(0, std::time::Duration::ZERO, 0); 5],
        }
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

    /// Return the compiled module for `wasm`, compiling it on a cache miss.
    fn get_or_compile(&mut self, wasm: &[u8]) -> Result<Module> {
        if let Some(m) = self.module_cache.get(wasm) {
            self.stats_cache_hits += 1;
            return Ok(m.clone());
        }
        let started = std::time::Instant::now();
        let module = Module::new(&self.engine, wasm)?;
        let took = started.elapsed();
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

        if self.module_cache.len() >= MODULE_CACHE_MAX {
            if let Some(old) = self.module_cache_order.pop_front() {
                self.module_cache.remove(&old);
            }
        }
        let key: std::sync::Arc<[u8]> = wasm.into();
        self.module_cache.insert(key.clone(), module.clone());
        self.module_cache_order.push_back(key);
        Ok(module)
    }

    pub fn flush_cache(&mut self) {
        let count = self.modules.len();
        self.modules.clear();
        if count > 0 {
            eprintln!("[qemu-jit] flushed {} compiled TBs from cache", count);
        }
    }

    /// Forget every compiled TB because the Store holding their instances is
    /// being replaced. The new QEMU instance starts with a fresh function
    /// table, so no slots are in use any more either.
    pub fn reset_for_new_store(&mut self) {
        self.modules.clear();
        self.free_slots.clear();
        self.instances_in_store = 0;
    }
}

// ---------------------------------------------------------------------------
//  Host state for the QEMU core-module Store
// ---------------------------------------------------------------------------

use crate::qemu_gfx::QemuGfxState;
use wasmtime_wasi::p1::WasiP1Ctx;

pub struct QemuHostState {
    pub wasi: WasiP1Ctx,
    pub jit: QemuJitState,
    pub gfx: QemuGfxState,
    /// Cached reference to the guest's linear memory (set after instantiation)
    pub guest_memory: Option<Memory>,
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
         -> u32 { jit_load(&mut caller, wasm_ptr, wasm_len, helpers_ptr, helpers_len, true) },
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
         -> u32 { jit_load(&mut caller, wasm_ptr, wasm_len, helpers_ptr, helpers_len, false) },
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
                    if let Some(wasmtime::Extern::Memory(mem)) = caller.get_export("memory") {
                        let data = mem.data_mut(&mut caller);
                        let p = ctx_ptr as usize;
                        if p + 12 <= data.len() {
                            data[p + 8..p + 12].copy_from_slice(&0u32.to_le_bytes());
                        }
                    }
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
                    if let Some(wasmtime::Extern::Memory(mem)) = caller.get_export("memory") {
                        let data = mem.data_mut(&mut caller);
                        let p = ctx_ptr as usize;
                        // wasmContext layout: env(0) stack(4) tb_ptr(8) ...
                        if p + 12 <= data.len() {
                            data[p + 8..p + 12].copy_from_slice(&0u32.to_le_bytes());
                        }
                    }
                    0
                }
            }
        },
    )?;

    // -----------------------------------------------------------------------
    //  env::wasi_jit_drop
    //
    //  void wasi_jit_drop(uint32_t module_handle)
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "wasi_jit_drop",
        |mut caller: Caller<'_, QemuHostState>, handle: u32| {
            if caller.data_mut().jit.modules.remove(&handle).is_none() {
                eprintln!("[qemu-jit] drop: unknown handle {handle}");
                return;
            }
            // Clear the TB's slot in QEMU's function table and recycle it.
            // (The instance itself lives until the Store is dropped.)
            if let Some(wasmtime::Extern::Table(t)) = caller.get_export("__indirect_function_table") {
                let _ = t.set(&mut caller, handle as u64, wasmtime::Ref::Func(None));
            }
            caller.data_mut().jit.free_slots.push(handle);
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

/// Load the TB module at `wasm_ptr` into the Store (see `wasi_jit_compile`)
/// and return its handle, or 0 on failure. With `compile` false, only a
/// module already in the module cache is used.
fn jit_load(
    caller: &mut Caller<'_, QemuHostState>,
    wasm_ptr: i32,
    wasm_len: i32,
    helpers_ptr: i32,
    helpers_len: i32,
    compile: bool,
) -> u32 {
    // Get guest memory
    let memory = match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(m)) => m,
        _ => {
            eprintln!("[qemu-jit] compile: guest memory not found");
            return 0;
        }
    };

    let data = memory.data(&*caller);

    // Read wasm bytes
    let ws = wasm_ptr as usize;
    let we = ws + wasm_len as usize;
    if we > data.len() {
        eprintln!(
            "[qemu-jit] compile: wasm bytes OOB ({ws}..{we}, mem {})",
            data.len()
        );
        return 0;
    }
    let wasm_bytes = data[ws..we].to_vec();

    // Read helper function indices (array of u32 LE)
    let hs = helpers_ptr as usize;
    let he = hs + (helpers_len as usize) * 4;
    if he > data.len() {
        eprintln!(
            "[qemu-jit] compile: helper indices OOB ({hs}..{he}, mem {})",
            data.len()
        );
        return 0;
    }
    let mut helper_indices: Vec<u32> = Vec::with_capacity(helpers_len as usize);
    for i in (hs..he).step_by(4) {
        let idx = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        helper_indices.push(idx);
    }

    if !compile && !caller.data().jit.module_cache.contains_key(&wasm_bytes[..]) {
        return 0;
    }

    // Compile the TB wasm module, or reuse an identical one compiled
    // before a Store migration.
    let engine = caller.data().jit.engine.clone();

    let module = match caller.data_mut().jit.get_or_compile(&wasm_bytes) {
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
    };

    // Resolve the TB module's imports:
    //   env.buffer : QEMU's linear memory
    //   env.table  : QEMU's function table. Current QEMU builds call
    //                helpers with call_indirect through it, so helper
    //                calls never leave wasm.
    //   helper.N   : per-helper host trampolines (older QEMU builds)
    let guest_table = match caller.get_export("__indirect_function_table") {
        Some(wasmtime::Extern::Table(t)) => t,
        _ => {
            eprintln!("[qemu-jit] compile: __indirect_function_table not found");
            return 0;
        }
    };
    let mut tb_linker: Linker<QemuHostState> = Linker::new(&engine);
    if let Err(e) = tb_linker.define(&*caller, "env", "buffer", memory) {
        eprintln!("[qemu-jit] compile: define env/buffer failed: {e}");
        return 0;
    }
    if let Err(e) = tb_linker.define(&*caller, "env", "table", guest_table) {
        eprintln!("[qemu-jit] compile: define env/table failed: {e}");
        return 0;
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
            return 0;
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
            return 0;
        }
    }

    // Instantiate the TB module
    let instance = match tb_linker.instantiate(caller.as_context_mut(), &module) {
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
    let handle = match caller.data_mut().jit.free_slots.pop() {
        Some(slot) => {
            if let Err(e) = guest_table.set(caller.as_context_mut(), slot as u64, entry) {
                eprintln!("[qemu-jit] compile: table set failed: {e}");
                return 0;
            }
            slot
        }
        None => match guest_table.grow(caller.as_context_mut(), 1, entry) {
            Ok(index) => index as u32,
            Err(e) => {
                eprintln!("[qemu-jit] compile: table grow failed: {e}");
                return 0;
            }
        },
    };

    let jit = &mut caller.data_mut().jit;
    jit.instances_in_store += 1;
    jit.modules.insert(
        handle,
        CompiledTB {
            instance,
            start_func,
        },
    );
    handle
}
