//! Multi-threaded QEMU: the WASI_THREADS build (`make THREADS=1` in
//! qemu-wasi), which runs each vCPU on a thread of its own (MTTCG).
//!
//! Such a QEMU imports its linear memory (`env.memory`), shared by all of
//! its threads. Each thread runs in a wasm instance of its own, in a Store of
//! its own, on an OS thread of its own:
//!
//! - QEMU creates threads with pthread_create(), which calls the
//!   `wasi.thread-spawn` import (the wasi-threads ABI): `QemuThreads::spawn`
//!   starts an OS thread, which instantiates QEMU's module again and calls
//!   its `wasi_thread_start` export.
//! - A vCPU thread then calls `env.host_vcpu_run`, and `run_vcpu` runs the
//!   vCPU in yet another Store: one iteration of its loop at a time, through
//!   QEMU's `wasi_vcpu_enter` export, on the same OS thread and stack. A
//!   Store keeps every TB module instantiated in it until it is dropped, and
//!   this is how it can be dropped and replaced from time to time, as the
//!   single-threaded QEMU's one is by `wasm::migrate_store`.
//! - All threads share one WASI context, which only one of them can use at
//!   a time (`SharedWasi`). Clocks, sleeps and random numbers, which need no
//!   context and are frequent, bypass it (`add_threads_to_linker`).
//! - Compiled TB modules (`JitCache`) and the display are shared, too.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use wasmtime::{CallHook, Caller, Engine, Instance, InstancePre, Linker, SharedMemory, Store};
use wasmtime_wasi::p1::WasiP1Ctx;

use crate::qemu_gfx::QemuGfxState;
use crate::qemu_jit::{with_guest, with_guest_mut, GuestMem, JitCache, QemuHostState, QemuJitState, WasiSlot};

/// Native stack of QEMU's threads: wasm frames live there, and a vCPU
/// thread nests its vCPU's Store in a host call of its own Store.
const THREAD_STACK: usize = 16 << 20;

/// `wasi_vcpu_enter` flag: the vCPU's Store is new, so it must forget the
/// handles of the TB modules it had (see QEMU's wasi_vcpu_step).
const VCPU_JIT_RESET: i32 = 1;

// WASI errno values.
const ERRNO_FAULT: i32 = 21;
const ERRNO_INVAL: i32 = 28;
const ERRNO_NOTSUP: i32 = 58;

// ---------------------------------------------------------------------------
//  The shared WASI context
// ---------------------------------------------------------------------------

/// The WASI context of all of QEMU's threads: open files (disk images...)
/// must be the same for all of them. wasmtime-wasi's functions take it as
/// `&mut WasiP1Ctx` from the calling Store's data, so it is handed out
/// under a lock that `QemuHostState::wasi_ctx` takes when a WASI function
/// asks for it, and the call hook of `install_store_hooks` releases when
/// that function returns to wasm.
pub struct SharedWasi {
    locked: AtomicBool,
    pub(crate) ctx: UnsafeCell<WasiP1Ctx>,
}

// SAFETY: `ctx` is only used by the thread holding `locked`.
unsafe impl Sync for SharedWasi {}

impl SharedWasi {
    pub fn new(ctx: WasiP1Ctx) -> Arc<Self> {
        Arc::new(Self { locked: AtomicBool::new(false), ctx: UnsafeCell::new(ctx) })
    }

    /// File operations are short, so spin, then yield.
    pub fn lock(&self) {
        let mut spins = 0u32;
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins < 100 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }

    pub fn unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

/// Make `store` release the shared WASI context's lock when a host call
/// returns. Needed for every Store whose WASI context is shared.
pub fn install_store_hooks(store: &mut Store<QemuHostState>) {
    store.call_hook(|mut ctx, hook| {
        if matches!(hook, CallHook::ReturningFromHost) {
            ctx.data_mut().release_wasi();
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------
//  Threads
// ---------------------------------------------------------------------------

/// What QEMU's threads share, and how to make a Store for a new one.
pub struct QemuThreads {
    engine: Engine,
    pre: OnceLock<InstancePre<QemuHostState>>,
    pub memory: SharedMemory,
    wasi: Arc<SharedWasi>,
    gfx: Arc<Mutex<QemuGfxState>>,
    jit: Arc<Mutex<JitCache>>,
    next_tid: AtomicI32,
    /// Incremented to move every vCPU thread to a fresh Store, when the
    /// process has too many memory mappings (see `run_vcpu`).
    migrate_epoch: AtomicU32,
}

impl QemuThreads {
    pub fn new(
        engine: &Engine,
        memory: SharedMemory,
        wasi: WasiP1Ctx,
        gfx: Arc<Mutex<QemuGfxState>>,
        jit: Arc<Mutex<JitCache>>,
    ) -> Arc<Self> {
        if !cfg!(target_arch = "x86_64") {
            eprintln!(
                "[qemu] warning: on this host, the vCPU threads may see each other's \
                 memory accesses out of order (QEMU assumes x86-64 ordering)"
            );
        }
        Arc::new(Self {
            engine: engine.clone(),
            pre: OnceLock::new(),
            memory,
            wasi: SharedWasi::new(wasi),
            gfx,
            jit,
            next_tid: AtomicI32::new(1),
            migrate_epoch: AtomicU32::new(0),
        })
    }

    /// QEMU's module, linked: set once the Linker is complete.
    pub fn set_instance_pre(&self, pre: InstancePre<QemuHostState>) {
        let _ = self.pre.set(pre);
    }

    /// A Store for one of QEMU's threads.
    pub fn new_store(&self) -> Store<QemuHostState> {
        let state = QemuHostState {
            wasi: WasiSlot::Shared(self.wasi.clone()),
            wasi_locked: false,
            jit: QemuJitState::new(self.jit.clone()),
            gfx: self.gfx.clone(),
            guest_memory: Some(GuestMem::Shared(self.memory.clone())),
            guest_table: None,
            pending_exception: false,
            limits: crate::qemu_jit::qemu_store_limits(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|s| &mut s.limits);
        install_store_hooks(&mut store);
        if crate::qemu_profile::configured() {
            // The engine checks epochs, but the profiler only follows the
            // main thread: never interrupt the others.
            store.set_epoch_deadline(u64::MAX / 2);
        }
        store
    }

    /// An instance of QEMU's module in `store`, sharing the memory.
    pub fn instantiate(&self, store: &mut Store<QemuHostState>) -> Result<Instance> {
        let pre = self.pre.get().context("QEMU's module is not linked yet")?;
        let instance = pre.instantiate(&mut *store)?;
        if let Some(wasmtime::Extern::Table(t)) =
            instance.get_export(&mut *store, "__indirect_function_table")
        {
            store.data_mut().guest_table = Some(t);
        }
        Ok(instance)
    }

    /// `wasi.thread-spawn`: run `wasi_thread_start(tid, start_arg)` on a new
    /// thread. Returns the thread's id, or a negative value.
    fn spawn(self: &Arc<Self>, start_arg: i32) -> i32 {
        let tid = self.next_tid.fetch_add(1, Ordering::Relaxed);
        let threads = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("qemu-{tid}"))
            .stack_size(THREAD_STACK)
            .spawn(move || {
                let mut store = threads.new_store();
                let result = threads.instantiate(&mut store).and_then(|instance| {
                    let start = instance
                        .get_typed_func::<(i32, i32), ()>(&mut store, "wasi_thread_start")?;
                    start.call(&mut store, (tid, start_arg))
                });
                store.data_mut().release_wasi();
                if let Err(e) = result {
                    thread_failed(&format!("thread {tid}"), e);
                }
            });
        match spawned {
            Ok(_) => tid,
            Err(e) => {
                eprintln!("[qemu] cannot start a thread: {e}");
                -6 // EAGAIN
            }
        }
    }

    /// `env.host_vcpu_run`, called by the vCPU thread of `cpu`: run that
    /// vCPU here until it is unplugged. Its Store is replaced when it holds
    /// too many TB modules, or when the process has too many memory mappings.
    fn run_vcpu(&self, cpu: i32, stack_top: u32, tls_base: u32) {
        use crate::wasm::{migrate_after_instances, process_map_count, MAP_CHECK_INTERVAL, MAP_COUNT_SOFT_LIMIT};

        let what = format!("vCPU {cpu}");
        let mut epoch = self.migrate_epoch.load(Ordering::Acquire);
        loop {
            let mut store = self.new_store();
            let enter = self
                .instantiate(&mut store)
                .and_then(|instance| {
                    instance.get_typed_func::<(i32, i32, i32), i32>(&mut store, "wasi_vcpu_enter")
                })
                .unwrap_or_else(|e| thread_failed(&what, e));
            let mut flags = VCPU_JIT_RESET;
            let mut next_map_check = MAP_CHECK_INTERVAL;
            let freed = loop {
                let result = enter.call(&mut store, (stack_top as i32, tls_base as i32, flags));
                store.data_mut().release_wasi();
                flags = 0;
                match result {
                    Ok(0) => {}
                    Ok(_) => return, // unplugged
                    Err(e) => thread_failed(&what, e),
                }
                let instances = store.data().jit.instances_in_store;
                if instances >= next_map_check {
                    next_map_check = instances + MAP_CHECK_INTERVAL;
                    if process_map_count().is_some_and(|n| n > MAP_COUNT_SOFT_LIMIT) {
                        self.migrate_epoch.fetch_add(1, Ordering::AcqRel);
                    }
                }
                let now = self.migrate_epoch.load(Ordering::Acquire);
                if instances >= migrate_after_instances() || now != epoch {
                    epoch = now;
                    break instances;
                }
            };
            // Dropping the Store is what frees its TB module instances.
            drop(store);
            eprintln!("[qemu] {what}: moved to a fresh store, {freed} TB modules freed");
            // The cached modules are what remains mapped now. If they alone
            // use most of the budget, moves would repeat constantly: trim.
            if process_map_count().is_some_and(|n| n > MAP_COUNT_SOFT_LIMIT * 2 / 3) {
                let mut cache = self.jit.lock().unwrap();
                let keep = cache.module_cache_len() / 2;
                cache.shrink_module_cache(keep);
            }
        }
    }
}

/// A thread of QEMU's trapped, or QEMU called exit() on it: wasi-threads
/// semantics are that the whole program ends.
fn thread_failed(what: &str, e: anyhow::Error) -> ! {
    if let Some(exit) = e.downcast_ref::<wasmtime_wasi::I32Exit>() {
        eprintln!("[qemu] QEMU exited with status {} ({what})", exit.0);
        std::process::exit(exit.0);
    }
    eprintln!("[qemu] {what} failed: {e:?}");
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
//  Imports
// ---------------------------------------------------------------------------

/// Add the imports of the multi-threaded QEMU: `wasi.thread-spawn`,
/// `env.host_vcpu_run`, and WASI functions that need not wait for the shared
/// WASI context. Call after `wasmtime_wasi::p1::add_to_linker_sync`.
pub fn add_threads_to_linker(linker: &mut Linker<QemuHostState>, threads: &Arc<QemuThreads>) -> Result<()> {
    let t = threads.clone();
    linker.func_wrap("wasi", "thread-spawn", move |start_arg: i32| -> i32 { t.spawn(start_arg) })?;

    let t = threads.clone();
    linker.func_wrap("env", "host_vcpu_run", move |cpu: i32, stack_top: i32, tls_base: i32| {
        t.run_vcpu(cpu, stack_top as u32, tls_base as u32)
    })?;

    // These replace wasmtime-wasi's: the vCPUs read the clock all the time
    // (the guest's rdtsc), and a sleep must not hold the WASI lock.
    linker.allow_shadowing(true);
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_time_get",
        |mut caller: Caller<'_, QemuHostState>, id: i32, _precision: i64, out: i32| -> i32 {
            let Some(now) = clock_now(id as u32) else { return ERRNO_INVAL };
            put_u64(&mut caller, out, now)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_res_get",
        |mut caller: Caller<'_, QemuHostState>, id: i32, out: i32| -> i32 {
            if clock_now(id as u32).is_none() {
                return ERRNO_INVAL;
            }
            put_u64(&mut caller, out, 1)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "random_get",
        |mut caller: Caller<'_, QemuHostState>, buf: i32, len: i32| -> i32 {
            use rand::RngCore;
            match with_guest_mut(&mut caller, buf as u32 as usize, len as u32 as usize, |b| {
                rand::rng().fill_bytes(b)
            }) {
                Some(()) => 0,
                None => ERRNO_FAULT,
            }
        },
    )?;
    linker.func_wrap("wasi_snapshot_preview1", "sched_yield", || -> i32 {
        std::thread::yield_now();
        0
    })?;
    linker.func_wrap("wasi_snapshot_preview1", "poll_oneoff", poll_oneoff)?;
    linker.allow_shadowing(false);
    Ok(())
}

fn put_u64(caller: &mut Caller<'_, QemuHostState>, ptr: i32, value: u64) -> i32 {
    match with_guest_mut(caller, ptr as u32 as usize, 8, |b| b.copy_from_slice(&value.to_le_bytes())) {
        Some(()) => 0,
        None => ERRNO_FAULT,
    }
}

/// The time on WASI clock `id`, in nanoseconds. All threads use these, so
/// they agree on the monotonic clock's origin. CPU-time clocks read as the
/// monotonic clock.
fn clock_now(id: u32) -> Option<u64> {
    static START: OnceLock<Instant> = OnceLock::new();
    match id {
        0 => Some(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_nanos() as u64),
        1..=3 => Some(START.get_or_init(Instant::now).elapsed().as_nanos() as u64 + 1_000_000_000),
        _ => None,
    }
}

/// `poll_oneoff`, for clock subscriptions only, which is how wasi-libc
/// sleeps (nanosleep, usleep): QEMU polls no file descriptors through it.
fn poll_oneoff(mut caller: Caller<'_, QemuHostState>, subs: i32, events: i32, nsubs: i32, nevents: i32) -> i32 {
    const SUB: usize = 48;
    const EVENT: usize = 32;
    let n = nsubs as u32 as usize;
    if n == 0 {
        return ERRNO_INVAL;
    }
    let Some(subs) = with_guest(&caller, subs as u32 as usize, n * SUB, |b| b.to_vec()) else {
        return ERRNO_FAULT;
    };
    let mut first: Option<(Instant, u64)> = None;
    for sub in subs.chunks_exact(SUB) {
        let userdata = u64::from_le_bytes(sub[0..8].try_into().unwrap());
        if sub[8] != 0 {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("[qemu] poll_oneoff on file descriptors is not supported");
            }
            return ERRNO_NOTSUP;
        }
        let clock = u32::from_le_bytes(sub[16..20].try_into().unwrap());
        let timeout = u64::from_le_bytes(sub[24..32].try_into().unwrap());
        let absolute = u16::from_le_bytes(sub[40..42].try_into().unwrap()) & 1 != 0;
        let Some(now) = clock_now(clock) else { return ERRNO_INVAL };
        let wait = if absolute { timeout.saturating_sub(now) } else { timeout };
        let deadline = Instant::now() + Duration::from_nanos(wait);
        if first.is_none_or(|(d, _)| deadline < d) {
            first = Some((deadline, userdata));
        }
    }
    let (deadline, userdata) = first.expect("at least one subscription");
    let wait = deadline.saturating_duration_since(Instant::now());
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }
    let mut event = [0u8; EVENT];
    event[0..8].copy_from_slice(&userdata.to_le_bytes());
    // error (u16 at 8) = 0, type (u8 at 10) = 0: the clock fired.
    if with_guest_mut(&mut caller, events as u32 as usize, EVENT, |b| b.copy_from_slice(&event)).is_none() {
        return ERRNO_FAULT;
    }
    match with_guest_mut(&mut caller, nevents as u32 as usize, 4, |b| b.copy_from_slice(&1u32.to_le_bytes())) {
        Some(()) => 0,
        None => ERRNO_FAULT,
    }
}
