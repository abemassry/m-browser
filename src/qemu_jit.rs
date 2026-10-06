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
    next_handle: u32,
    /// TB module instances created in the current Store. Wasmtime frees
    /// instances only when their Store is dropped, so this grows until the
    /// runtime moves QEMU into a fresh Store (see `wasm::migrate_store`).
    pub instances_in_store: u32,
}

impl QemuJitState {
    pub fn new(engine: &Engine) -> Self {
        Self {
            engine: engine.clone(),
            modules: HashMap::new(),
            next_handle: 1,
            instances_in_store: 0,
        }
    }

    pub fn flush_cache(&mut self) {
        let count = self.modules.len();
        self.modules.clear();
        if count > 0 {
            eprintln!("[qemu-jit] flushed {} compiled TBs from cache", count);
        }
    }

    /// Forget every compiled TB because the Store holding their instances is
    /// being replaced. Handles keep increasing so stale ones never alias.
    pub fn reset_for_new_store(&mut self) {
        self.modules.clear();
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
         -> u32 {
            // Get guest memory
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(m)) => m,
                _ => {
                    eprintln!("[qemu-jit] compile: guest memory not found");
                    return 0;
                }
            };

            let data = memory.data(&caller);

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

            // Compile the TB wasm module
            let engine = caller.data().jit.engine.clone();

            // Dump TBs around the known-failing range for offline analysis
            let probable_handle = caller.data().jit.next_handle;
            if (245..=255).contains(&probable_handle) {
                let path = format!("/tmp/tb_h{}.wasm", probable_handle);
                let _ = std::fs::write(&path, &wasm_bytes);
                eprintln!("[qemu-jit] dumped {} ({} bytes)", path, wasm_bytes.len());
            }

            let module = match Module::new(&engine, &wasm_bytes) {
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

            // Build imports for the TB module.
            // TB modules expect:
            //   - "env"/"memory" -> shared linear memory from the guest
            //   - "helper"/"0", "helper"/"1", ... -> call into guest's
            //     indirect function table
            let mut tb_linker: Linker<QemuHostState> = Linker::new(&engine);

            // Shared memory
            if let Err(e) = tb_linker.define(&caller, "env", "buffer", memory) {
                eprintln!("[qemu-jit] compile: define env/buffer failed: {e}");
                return 0;
            }

            // Helper trampolines — look up the guest's __indirect_function_table
            // and create dynamic-signature forwarding functions that match
            // each TB module import's actual type.
            let guest_table = match caller.get_export("__indirect_function_table") {
                Some(wasmtime::Extern::Table(t)) => t,
                _ => {
                    eprintln!("[qemu-jit] compile: __indirect_function_table not found");
                    return 0;
                }
            };

            // Collect the expected types for each helper import from the TB module
            let import_types: Vec<(String, FuncType)> = module
                .imports()
                .filter(|imp| imp.module() == "helper")
                .map(|imp| {
                    let ft = imp
                        .ty()
                        .func()
                        .expect("helper import must be a function")
                        .clone();
                    (imp.name().to_string(), ft)
                })
                .collect();

            // Log helper info for first TB only (debugging)
            let handle_for_log = caller.data().jit.next_handle;
            if handle_for_log == 1 {
                eprintln!(
                    "[qemu-jit] compile h={}: {} helper imports, {} helper_indices",
                    handle_for_log,
                    import_types.len(),
                    helper_indices.len()
                );
            }

            // Check table size
            let table_size = guest_table.size(&caller);

            for (i, &table_index) in helper_indices.iter().enumerate() {
                let helper_name = i.to_string();
                let table = guest_table;

                // Validate table index
                if (table_index as u64) >= table_size as u64 {
                    eprintln!(
                        "[qemu-jit] WARNING: helper {} table_index {} >= table_size {}",
                        i, table_index, table_size
                    );
                }

                // Find the expected type for this helper
                let expected_ty = import_types
                    .iter()
                    .find(|(name, _)| name == &helper_name)
                    .map(|(_, ft)| ft.clone());

                let func_type = match expected_ty {
                    Some(ft) => ft,
                    None => {
                        // Fallback: single i32 param, single i32 result
                        FuncType::new(
                            caller.as_context_mut().engine(),
                            [ValType::I32],
                            [ValType::I32],
                        )
                    }
                };

                let trampoline = Func::new(
                    caller.as_context_mut(),
                    func_type,
                    move |mut caller: Caller<'_, QemuHostState>,
                          params: &[Val],
                          results: &mut [Val]|
                          -> Result<()> {
                        let func_ref = table
                            .get(&mut caller, table_index as u64)
                            .context("helper table index OOB")?;
                        let func = match func_ref {
                            wasmtime::Ref::Func(Some(f)) => f,
                            wasmtime::Ref::Func(None) => {
                                anyhow::bail!("table entry {} is null funcref", table_index)
                            }
                            _ => {
                                anyhow::bail!("table entry {} is not a funcref", table_index)
                            }
                        };

                        // Call with the exact parameter types the TB expects
                        let mut call_results = vec![Val::I32(0); results.len()];
                        match func.call(&mut caller, params, &mut call_results) {
                            Ok(()) => {
                                for (r, v) in results.iter_mut().zip(call_results.iter()) {
                                    *r = v.clone();
                                }
                                Ok(())
                            }
                            Err(e) => {
                                // Helper threw a wasm exception (e.g., siglongjmp from
                                // cpu_loop_exit inside helper_raise_exception). Mark
                                // the host state so the C wrapper can convert this
                                // into a proper cpu_loop_exit on the host module side.
                                caller.data_mut().pending_exception = true;
                                Err(e)
                            }
                        }
                    },
                );

                if let Err(e) = tb_linker.define(&caller, "helper", &helper_name, trampoline) {
                    eprintln!("[qemu-jit] compile: define helper/{helper_name} failed: {e}");
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

            // Get the "start" export
            let start_func =
                match instance.get_typed_func::<(i32,), (i32,)>(caller.as_context_mut(), "start") {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("[qemu-jit] compile: 'start' export not found: {e}");
                        return 0;
                    }
                };

            // Assign handle
            let jit = &mut caller.data_mut().jit;
            let handle = jit.next_handle;
            jit.next_handle += 1;
            jit.instances_in_store += 1;
            jit.modules.insert(
                handle,
                CompiledTB {
                    instance,
                    start_func,
                },
            );

            if handle == 1 || handle % 100 == 0 {
                eprintln!(
                    "[qemu-jit] compiled TB handle={handle} ({} bytes wasm, {} helpers)",
                    wasm_bytes.len(),
                    helper_indices.len()
                );
            }
            handle
        },
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
