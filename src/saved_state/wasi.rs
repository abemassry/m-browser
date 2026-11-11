//! WASI preview 1 in saved states: the files a program has open, and its
//! monotonic clock.
//!
//! wasmtime-wasi keeps a program's open files in its `WasiP1Ctx`, which
//! cannot be saved: it holds host file handles. So m-browser keeps a
//! journal of what the program opened (`FileJournal`), by wrapping
//! `path_open`, `fd_close` and `fd_renumber` in the Linker
//! (`add_to_linker`). To resume, the same files are opened again in a new
//! context that has the same preopened directories, under the same
//! descriptor numbers and at the same offsets (`restore`).
//!
//! The wrappers run wasmtime-wasi's own implementation, as its Linker glue
//! does (`call_p1`). Some more WASI functions are wrapped for the runner's
//! sake: before any wrapped function runs, the runner gets a say
//! (`SavePointHook`). It may pause the program there, or, in a program
//! transformed by Asyncify, unwind its stack to save it.
//!
//! **Pitfall:** `wasmtime_wasi::p1::wasi_snapshot_preview1` is code
//! generated inside wasmtime-wasi, which says it is not meant for use
//! outside. It is public, and Cargo.toml pins the versions; a wasmtime
//! upgrade may change it, which would show when compiling.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use wasmtime::{AsContextMut, Caller, Extern, Linker};
use wasmtime_wasi::p1::wasi_snapshot_preview1::{self as p1, WasiSnapshotPreview1};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::runtime::in_tokio;
use wiggle::GuestMemory;

const OFLAGS_CREAT: u32 = 1;
const OFLAGS_DIRECTORY: u32 = 2;
const OFLAGS_EXCL: u32 = 4;
const OFLAGS_TRUNC: u32 = 8;
const RIGHTS_FD_READ: u64 = 1 << 1;
const FILETYPE_REGULAR_FILE: u8 = 4;

/// The store data of a program whose WASI state can be saved.
pub trait WasiHost: 'static {
    fn wasi(&mut self) -> &mut WasiP1Ctx;
    fn journal(&mut self) -> &mut FileJournal;
}

/// The runner's say before a wrapped WASI function runs, given its name:
/// `Ok(None)` runs it, `Ok(Some(errno))` returns that instead of running it
/// (while the program unwinds its stack for a save), and `Err` stops the
/// program with a trap.
pub type SavePointHook<T> = fn(&mut Caller<'_, T>, &'static str) -> Result<Option<i32>>;

/// Wrap WASI functions of a Linker that has wasmtime-wasi's already
/// (`wasmtime_wasi::p1::add_to_linker_sync`): the three that open and close
/// files, for the journal, and the ones programs call while they wait or
/// work (reading, writing, sleeping, clocks, random numbers), where the
/// runner can pause or save them.
pub fn add_to_linker<T: WasiHost>(linker: &mut Linker<T>, hook: SavePointHook<T>) -> Result<()> {
    linker.allow_shadowing(true);
    let result = add_wrappers(linker, hook);
    linker.allow_shadowing(false);
    result
}

/// A wrapped function that only gives the runner its say.
macro_rules! save_point {
    ($linker:ident, $hook:ident, $name:ident($($arg:ident: $ty:ty),*) $(, $async:tt)?) => {
        $linker.func_wrap(
            "wasi_snapshot_preview1",
            stringify!($name),
            move |mut caller: Caller<'_, T>, $($arg: $ty),*| -> Result<i32> {
                if let Some(errno) = $hook(&mut caller, stringify!($name))? {
                    return Ok(errno);
                }
                call_p1(&mut caller, |ctx, mem| save_point!(@run $($async)? p1::$name(ctx, mem, $($arg),*)))
            },
        )?;
    };
    (@run async $call:expr) => { in_tokio($call) };
    (@run $call:expr) => { $call };
}

fn add_wrappers<T: WasiHost>(linker: &mut Linker<T>, hook: SavePointHook<T>) -> Result<()> {
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "path_open",
        move |mut caller: Caller<'_, T>,
              dirfd: i32,
              dirflags: i32,
              path: i32,
              path_len: i32,
              oflags: i32,
              rights_base: i64,
              rights_inheriting: i64,
              fdflags: i32,
              opened: i32|
              -> Result<i32> {
            if let Some(errno) = hook(&mut caller, "path_open")? {
                return Ok(errno);
            }
            let errno = call_p1(&mut caller, |ctx, mem| {
                in_tokio(p1::path_open(
                    ctx, mem, dirfd, dirflags, path, path_len, oflags, rights_base, rights_inheriting, fdflags,
                    opened,
                ))
            })?;
            if errno == 0 {
                // It succeeded, so the path and the new descriptor are in memory.
                let name = read_guest(&mut caller, path, path_len);
                let fd = read_guest(&mut caller, opened, 4);
                if let (Some(name), Some(fd)) = (name, fd) {
                    let how = How {
                        dirflags: dirflags as u32,
                        oflags: oflags as u32,
                        rights_base: rights_base as u64,
                        rights_inheriting: rights_inheriting as u64,
                        fdflags: fdflags as u32,
                    };
                    let fd = u32::from_le_bytes(fd.try_into().expect("4 bytes"));
                    caller.data_mut().journal().opened(fd, dirfd as u32, &String::from_utf8_lossy(&name), how);
                }
            }
            Ok(errno)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_close",
        move |mut caller: Caller<'_, T>, fd: i32| -> Result<i32> {
            if let Some(errno) = hook(&mut caller, "fd_close")? {
                return Ok(errno);
            }
            let errno = call_p1(&mut caller, |ctx, mem| in_tokio(p1::fd_close(ctx, mem, fd)))?;
            if errno == 0 {
                caller.data_mut().journal().closed(fd as u32);
            }
            Ok(errno)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_renumber",
        move |mut caller: Caller<'_, T>, from: i32, to: i32| -> Result<i32> {
            if let Some(errno) = hook(&mut caller, "fd_renumber")? {
                return Ok(errno);
            }
            let errno = call_p1(&mut caller, |ctx, mem| p1::fd_renumber(ctx, mem, from, to))?;
            if errno == 0 {
                caller.data_mut().journal().renumbered(from as u32, to as u32);
            }
            Ok(errno)
        },
    )?;
    save_point!(linker, hook, fd_read(fd: i32, iovs: i32, iovs_len: i32, nread: i32), async);
    save_point!(linker, hook, fd_write(fd: i32, iovs: i32, iovs_len: i32, nwritten: i32), async);
    save_point!(linker, hook, fd_pread(fd: i32, iovs: i32, iovs_len: i32, offset: i64, nread: i32), async);
    save_point!(linker, hook, fd_pwrite(fd: i32, iovs: i32, iovs_len: i32, offset: i64, nwritten: i32), async);
    save_point!(linker, hook, poll_oneoff(subs: i32, events: i32, nsubs: i32, nevents: i32), async);
    save_point!(linker, hook, clock_time_get(id: i32, precision: i64, time: i32));
    save_point!(linker, hook, random_get(buf: i32, len: i32));
    save_point!(linker, hook, sched_yield());
    Ok(())
}

/// Run one of wasmtime-wasi's preview-1 functions for the calling program,
/// on its memory and WASI context, as wasmtime-wasi's Linker glue does.
fn call_p1<T: WasiHost, R>(
    caller: &mut Caller<'_, T>,
    f: impl FnOnce(&mut WasiP1Ctx, &mut GuestMemory<'_>) -> Result<R>,
) -> Result<R> {
    let fuel = caller.as_context_mut().hostcall_fuel();
    match caller.get_export("memory") {
        Some(Extern::Memory(memory)) => {
            let (memory, data) = memory.data_and_store_mut(&mut *caller);
            let ctx = data.wasi();
            ctx.set_hostcall_fuel(fuel);
            f(ctx, &mut GuestMemory::Unshared(memory))
        }
        Some(Extern::SharedMemory(memory)) => {
            let ctx = caller.data_mut().wasi();
            ctx.set_hostcall_fuel(fuel);
            f(ctx, &mut GuestMemory::Shared(memory.data()))
        }
        _ => bail!("missing required memory export"),
    }
}

fn read_guest<T>(caller: &mut Caller<'_, T>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory")?.into_memory()?;
    let start = ptr as u32 as usize;
    let end = start.checked_add(len as u32 as usize)?;
    memory.data(&*caller).get(start..end).map(<[u8]>::to_vec)
}

/// How a file was opened, besides its name.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct How {
    pub dirflags: u32,
    pub oflags: u32,
    pub rights_base: u64,
    pub rights_inheriting: u64,
    pub fdflags: u32,
}

/// A file or directory the program opened, as it would open it again.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct OpenFile {
    pub fd: u32,
    /// The preopened directory it lies in (0 for the first one).
    pub preopen: u32,
    /// Its path in that directory.
    pub path: String,
    pub how: How,
}

/// What a program has open, besides standard I/O and the preopened
/// directories it started with.
#[derive(Clone, Debug, Default)]
pub struct FileJournal {
    files: BTreeMap<u32, OpenFile>,
    /// Descriptors the context starts with: 0 to 2 for standard I/O, then
    /// one per preopened directory.
    initial: u32,
    /// Standard I/O descriptors the program closed or replaced.
    closed_initial: BTreeSet<u32>,
    /// The highest descriptor number used so far.
    highest: u32,
    /// Why the program's descriptors cannot be saved, if they cannot.
    problem: Option<String>,
}

impl FileJournal {
    pub fn new(preopens: usize) -> Self {
        let initial = 3 + preopens as u32;
        FileJournal { initial, highest: initial - 1, ..Default::default() }
    }

    fn is_preopen(&self, fd: u32) -> bool {
        (3..self.initial).contains(&fd)
    }

    /// Why the program's open files cannot be saved, if they cannot.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    fn opened(&mut self, fd: u32, dirfd: u32, path: &str, how: How) {
        // Remember where it lies as a path in a preopened directory, so that
        // reopening it needs no other descriptor.
        let (preopen, path) = if self.is_preopen(dirfd) {
            (dirfd - 3, path.to_string())
        } else if let Some(dir) = self.files.get(&dirfd) {
            (dir.preopen, format!("{}/{path}", dir.path.trim_end_matches('/')))
        } else {
            self.problem
                .get_or_insert_with(|| format!("{path} was opened in descriptor {dirfd}, which m-browser cannot follow"));
            return;
        };
        self.highest = self.highest.max(fd);
        self.files.insert(fd, OpenFile { fd, preopen, path, how });
    }

    fn closed(&mut self, fd: u32) {
        if self.files.remove(&fd).is_some() {
            return;
        }
        if self.is_preopen(fd) {
            self.problem.get_or_insert_with(|| format!("the program closed its preopened directory {fd}"));
        } else if fd < 3 {
            self.closed_initial.insert(fd);
        }
    }

    fn renumbered(&mut self, from: u32, to: u32) {
        if from == to {
            return;
        }
        self.closed(to);
        match self.files.remove(&from) {
            Some(mut file) => {
                file.fd = to;
                self.highest = self.highest.max(to);
                self.files.insert(to, file);
            }
            None if from < self.initial => {
                self.problem.get_or_insert_with(|| {
                    format!("descriptor {from} (standard I/O or a preopened directory) was renumbered to {to}")
                });
            }
            None => {}
        }
    }
}

/// The WASI part of a saved state.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct WasiState {
    /// The program's monotonic clock when it was saved, in nanoseconds.
    pub monotonic_ns: u64,
    pub files: Vec<SavedFile>,
    /// Standard I/O descriptors the program had closed.
    pub closed_initial: Vec<u32>,
    pub highest: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedFile {
    pub open: OpenFile,
    /// The descriptor's offset, for regular files.
    pub position: Option<u64>,
    /// The file as it was when the state was saved.
    pub identity: FileIdentity,
}

/// What tells whether a file is still the one a state was saved with.
/// Directories only need to be the same directory; files must also have
/// the same size and modification time, or resuming would work on data the
/// program never saw (and, for a disk image, corrupt it).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FileIdentity {
    pub host_path: String,
    pub device: u64,
    pub inode: u64,
    /// Size and modification time (ns since 1970), for files.
    pub contents: Option<(u64, u128)>,
}

impl FileIdentity {
    pub(crate) fn of(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path).with_context(|| format!("cannot look at {}", path.display()))?;
        #[cfg(unix)]
        let (device, inode) = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let (device, inode) = (0, 0);
        let contents = (!meta.is_dir()).then(|| {
            let modified = meta.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok());
            (meta.len(), modified.map_or(0, |d| d.as_nanos()))
        });
        Ok(FileIdentity { host_path: path.to_string_lossy().into_owned(), device, inode, contents })
    }

    /// How `now` differs from `self`, if it does.
    pub(crate) fn change(&self, now: &FileIdentity) -> Option<&'static str> {
        if (now.device, now.inode) != (self.device, self.inode) {
            Some("it was replaced by another file")
        } else if now.contents.map(|c| c.0) != self.contents.map(|c| c.0) {
            Some("its size changed")
        } else if now.contents != self.contents {
            Some("it was modified")
        } else {
            None
        }
    }
}

/// The WASI state of a program at a save point: where its files are, and
/// its clock. `preopens` are the context's preopened directories, host
/// path first.
pub fn save(
    ctx: &mut WasiP1Ctx,
    journal: &FileJournal,
    preopens: &[(String, String)],
    clock: &ProgramClock,
) -> Result<WasiState> {
    if let Some(problem) = &journal.problem {
        bail!("its open files cannot be saved: {problem}");
    }
    let mut files = Vec::with_capacity(journal.files.len());
    for file in journal.files.values() {
        let (host_dir, _) = preopens.get(file.preopen as usize).context("the file journal is inconsistent")?;
        let identity = FileIdentity::of(&Path::new(host_dir).join(&file.path))?;
        let position = if host_filetype(ctx, file.fd)? == FILETYPE_REGULAR_FILE {
            Some(host_tell(ctx, file.fd)?)
        } else {
            None
        };
        files.push(SavedFile { open: file.clone(), position, identity });
    }
    Ok(WasiState {
        monotonic_ns: clock.now_ns(),
        files,
        closed_initial: journal.closed_initial.iter().copied().collect(),
        highest: journal.highest,
    })
}

/// Open the files of a saved state again in `ctx`, a new context with the
/// same preopened directories, under the same descriptor numbers and at
/// the same offsets. Refuses if a file changed since the state was saved.
///
/// wasmtime-wasi gives a new descriptor the highest free number below the
/// highest one used so far, else the next unused one. So the files are
/// opened in ascending order while nothing is free, with placeholders
/// filling the gaps; closing the placeholders at the end leaves the same
/// numbers free as in the saved program, and its next file gets the same
/// number it would have got.
pub fn restore(ctx: &mut WasiP1Ctx, state: &WasiState, preopens: usize) -> Result<FileJournal> {
    for file in &state.files {
        let now = FileIdentity::of(Path::new(&file.identity.host_path))?;
        if let Some(change) = file.identity.change(&now) {
            bail!(
                "{} changed since the state was saved ({change}); resuming could corrupt it",
                file.identity.host_path
            );
        }
    }
    let mut journal = FileJournal::new(preopens);
    let mut files = state.files.clone();
    files.sort_by_key(|file| file.open.fd);
    let mut placeholders = Vec::new();
    for file in &files {
        let open = &file.open;
        let target = open.fd;
        if target < 3 {
            // The program closed this standard I/O descriptor, and the
            // number went to this file.
            host_close(ctx, target)?;
            journal.closed_initial.insert(target);
        }
        // Reopening must neither create, nor fail on, nor empty the file.
        let oflags = open.how.oflags & !(OFLAGS_CREAT | OFLAGS_EXCL | OFLAGS_TRUNC);
        loop {
            let fd = host_open(ctx, 3 + open.preopen, open.how.dirflags, &open.path, oflags, &open.how)
                .with_context(|| format!("cannot open {} again", file.identity.host_path))?;
            if fd == target {
                break;
            }
            if fd > target {
                bail!("cannot open {} again as descriptor {target}: got {fd}", file.identity.host_path);
            }
            placeholders.push(fd);
        }
        if let Some(position) = file.position {
            host_seek(ctx, target, position)?;
        }
        journal.files.insert(target, open.clone());
    }
    let mut next = journal.initial.max(files.last().map_or(0, |file| file.open.fd + 1));
    while next <= state.highest {
        let how = How { dirflags: 0, oflags: OFLAGS_DIRECTORY, rights_base: RIGHTS_FD_READ, rights_inheriting: 0, fdflags: 0 };
        let fd = host_open(ctx, 3, 0, ".", OFLAGS_DIRECTORY, &how)?;
        placeholders.push(fd);
        next = fd + 1;
    }
    for fd in placeholders {
        host_close(ctx, fd)?;
    }
    for &fd in &state.closed_initial {
        if !journal.files.contains_key(&fd) {
            host_close(ctx, fd)?;
        }
        journal.closed_initial.insert(fd);
    }
    journal.highest = state.highest;
    Ok(journal)
}

/// Run a preview-1 function from the host, outside any call from wasm: the
/// paths it reads and the results it writes are in `scratch`, not in the
/// program's memory.
fn host_call(
    ctx: &mut WasiP1Ctx,
    scratch: &mut [u8],
    f: impl FnOnce(&mut WasiP1Ctx, &mut GuestMemory<'_>) -> Result<i32>,
) -> Result<()> {
    // How many bytes a call may copy out of "guest" memory. The Linker's
    // glue sets it before every call from wasm; without it, reading a path
    // fails with ENOMEM.
    ctx.set_hostcall_fuel(1 << 20);
    let errno = f(ctx, &mut GuestMemory::Unshared(scratch))?;
    if errno != 0 {
        bail!("{}", errno_text(errno));
    }
    Ok(())
}

fn host_open(ctx: &mut WasiP1Ctx, dirfd: u32, dirflags: u32, path: &str, oflags: u32, how: &How) -> Result<u32> {
    let mut scratch = vec![0u8; 8 + path.len()];
    scratch[8..].copy_from_slice(path.as_bytes());
    host_call(ctx, &mut scratch, |ctx, mem| {
        in_tokio(p1::path_open(
            ctx,
            mem,
            dirfd as i32,
            dirflags as i32,
            8,
            path.len() as i32,
            oflags as i32,
            how.rights_base as i64,
            how.rights_inheriting as i64,
            how.fdflags as i32,
            0,
        ))
    })?;
    Ok(u32::from_le_bytes(scratch[..4].try_into().expect("4 bytes")))
}

fn host_close(ctx: &mut WasiP1Ctx, fd: u32) -> Result<()> {
    host_call(ctx, &mut [], |ctx, mem| in_tokio(p1::fd_close(ctx, mem, fd as i32)))
        .with_context(|| format!("cannot close descriptor {fd}"))
}

fn host_seek(ctx: &mut WasiP1Ctx, fd: u32, position: u64) -> Result<()> {
    let mut scratch = [0u8; 8];
    host_call(ctx, &mut scratch, |ctx, mem| in_tokio(p1::fd_seek(ctx, mem, fd as i32, position as i64, 0, 0)))
        .with_context(|| format!("cannot seek descriptor {fd}"))
}

fn host_tell(ctx: &mut WasiP1Ctx, fd: u32) -> Result<u64> {
    let mut scratch = [0u8; 8];
    host_call(ctx, &mut scratch, |ctx, mem| p1::fd_tell(ctx, mem, fd as i32, 0))
        .with_context(|| format!("cannot get the offset of descriptor {fd}"))?;
    Ok(u64::from_le_bytes(scratch))
}

/// The WASI file type of a descriptor (`fd_fdstat_get`).
fn host_filetype(ctx: &mut WasiP1Ctx, fd: u32) -> Result<u8> {
    let mut scratch = [0u8; 24];
    host_call(ctx, &mut scratch, |ctx, mem| in_tokio(p1::fd_fdstat_get(ctx, mem, fd as i32, 0)))
        .with_context(|| format!("cannot look at descriptor {fd}"))?;
    Ok(scratch[0])
}

fn errno_text(errno: i32) -> String {
    let name = match errno {
        2 => "permission denied (EACCES)",
        8 => "bad descriptor (EBADF)",
        20 => "already exists (EEXIST)",
        28 => "invalid argument (EINVAL)",
        44 => "no such file or directory (ENOENT)",
        48 => "out of memory (ENOMEM)",
        54 => "not a directory (ENOTDIR)",
        63 => "not permitted (EPERM)",
        76 => "not allowed by the sandbox (ENOTCAPABLE)",
        _ => return format!("WASI errno {errno}"),
    };
    name.to_string()
}

/// The program's monotonic clock (WASI's `CLOCK_MONOTONIC`). It continues
/// from the value it had when the program was saved, and stands still
/// while the program is paused: a resumed or paused program never sees
/// this clock jump or go back. wasmtime-wasi's own clock would start near
/// 0 in every process.
#[derive(Clone)]
pub struct ProgramClock(Arc<Mutex<ClockState>>);

struct ClockState {
    origin: Instant,
    base_ns: u64,
    paused_for: Duration,
    paused_since: Option<Instant>,
}

impl ProgramClock {
    pub fn new() -> Self {
        ProgramClock(Arc::new(Mutex::new(ClockState {
            origin: Instant::now(),
            base_ns: 0,
            paused_for: Duration::ZERO,
            paused_since: None,
        })))
    }

    pub fn now_ns(&self) -> u64 {
        let state = self.0.lock().unwrap();
        let now = state.paused_since.unwrap_or_else(Instant::now);
        let ran = now.saturating_duration_since(state.origin).saturating_sub(state.paused_for);
        state.base_ns.saturating_add(ran.as_nanos() as u64)
    }

    /// Continue from `ns`.
    pub fn set_ns(&self, ns: u64) {
        let mut state = self.0.lock().unwrap();
        let now = Instant::now();
        state.origin = now;
        state.base_ns = ns;
        state.paused_for = Duration::ZERO;
        if state.paused_since.is_some() {
            state.paused_since = Some(now);
        }
    }

    pub fn pause(&self) {
        let mut state = self.0.lock().unwrap();
        state.paused_since.get_or_insert_with(Instant::now);
    }

    pub fn resume(&self) {
        let mut state = self.0.lock().unwrap();
        if let Some(since) = state.paused_since.take() {
            state.paused_for += since.elapsed();
        }
    }
}

impl Default for ProgramClock {
    fn default() -> Self {
        Self::new()
    }
}

impl wasmtime_wasi::HostMonotonicClock for ProgramClock {
    fn resolution(&self) -> u64 {
        1
    }

    fn now(&self) -> u64 {
        self.now_ns()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saved_state::tests::TestDir;
    use wasmtime::{Engine, Module, Store};

    const FILETYPE_DIRECTORY: u8 = 3;

    struct Host {
        wasi: WasiP1Ctx,
        journal: FileJournal,
    }

    impl WasiHost for Host {
        fn wasi(&mut self) -> &mut WasiP1Ctx {
            &mut self.wasi
        }
        fn journal(&mut self) -> &mut FileJournal {
            &mut self.journal
        }
    }

    fn run_everything(_: &mut Caller<'_, Host>, _: &'static str) -> Result<Option<i32>> {
        Ok(None)
    }

    fn context(dir: &Path) -> WasiP1Ctx {
        let mut builder = wasmtime_wasi::WasiCtxBuilder::new();
        builder
            .preopened_dir(dir, "/data", wasmtime_wasi::DirPerms::all(), wasmtime_wasi::FilePerms::all())
            .unwrap();
        builder.build_p1()
    }

    /// Opens a.txt, b.txt and c.txt, writes to a, closes b, opens the
    /// directory sub (which gets b's number) and sub/x.txt in it, writes to
    /// that, and closes c.
    const FILES: &str = r#"(module
        (import "wasi_snapshot_preview1" "path_open"
          (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_close" (func $fd_close (param i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 100) "a.txt")
        (data (i32.const 110) "b.txt")
        (data (i32.const 120) "c.txt")
        (data (i32.const 130) "sub")
        (data (i32.const 140) "x.txt")
        (data (i32.const 150) "hello")
        (data (i32.const 160) "abc")
        (func $open (param $dir i32) (param $path i32) (param $len i32) (param $oflags i32) (result i32)
          ;; Directories cannot be opened for writing (EISDIR): no FD_WRITE for them.
          (if (call $path_open (local.get $dir) (i32.const 1) (local.get $path) (local.get $len)
                (local.get $oflags)
                (select (i64.const 0x1fffffbf) (i64.const 0x1fffffff) (i32.and (local.get $oflags) (i32.const 2)))
                (i64.const 0x1fffffff) (i32.const 0) (i32.const 0))
            (then unreachable))
          (i32.load (i32.const 0)))
        (func $write (param $fd i32) (param $buf i32) (param $len i32)
          (i32.store (i32.const 16) (local.get $buf))
          (i32.store (i32.const 20) (local.get $len))
          (if (call $fd_write (local.get $fd) (i32.const 16) (i32.const 1) (i32.const 24)) (then unreachable)))
        (func (export "run")
          (local $a i32) (local $b i32) (local $c i32) (local $sub i32) (local $x i32)
          (local.set $a (call $open (i32.const 3) (i32.const 100) (i32.const 5) (i32.const 9)))
          (local.set $b (call $open (i32.const 3) (i32.const 110) (i32.const 5) (i32.const 9)))
          (local.set $c (call $open (i32.const 3) (i32.const 120) (i32.const 5) (i32.const 9)))
          (call $write (local.get $a) (i32.const 150) (i32.const 5))
          (drop (call $fd_close (local.get $b)))
          (local.set $sub (call $open (i32.const 3) (i32.const 130) (i32.const 3) (i32.const 2)))
          (local.set $x (call $open (local.get $sub) (i32.const 140) (i32.const 5) (i32.const 9)))
          (call $write (local.get $x) (i32.const 160) (i32.const 3))
          (drop (call $fd_close (local.get $c)))))"#;

    /// Run FILES in a new context and return its state, journal and clock.
    fn run_files(dir: &Path) -> (WasiState, Host) {
        let engine = Engine::default();
        let mut linker: Linker<Host> = Linker::new(&engine);
        wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |h: &mut Host| &mut h.wasi).unwrap();
        add_to_linker(&mut linker, run_everything).unwrap();
        let module = Module::new(&engine, FILES).unwrap();
        let mut store = Store::new(&engine, Host { wasi: context(dir), journal: FileJournal::new(1) });
        let instance = linker.instantiate(&mut store, &module).unwrap();
        instance.get_typed_func::<(), ()>(&mut store, "run").unwrap().call(&mut store, ()).unwrap();
        let preopens = [(dir.to_string_lossy().into_owned(), "/data".to_string())];
        let mut host = store.into_data();
        let state = save(&mut host.wasi, &host.journal, &preopens, &ProgramClock::new()).unwrap();
        (state, host)
    }

    #[test]
    fn files_are_reopened_under_the_same_numbers() {
        let dir = TestDir::new("wasi-files");
        std::fs::create_dir(dir.0.join("sub")).unwrap();
        let (state, mut original) = run_files(&dir.0);
        let fds: Vec<(u32, &str, Option<u64>)> =
            state.files.iter().map(|f| (f.open.fd, f.open.path.as_str(), f.position)).collect();
        assert_eq!(fds, [(4, "a.txt", Some(5)), (5, "sub", None), (7, "sub/x.txt", Some(3))]);
        assert_eq!(state.highest, 7);

        // As if in a new process: through the file format, into a new context.
        let state: WasiState = postcard::from_bytes(&postcard::to_allocvec(&state).unwrap()).unwrap();
        let mut ctx = context(&dir.0);
        let journal = restore(&mut ctx, &state, 1).unwrap();
        assert_eq!(journal.files, original.journal.files);
        assert_eq!(host_tell(&mut ctx, 4).unwrap(), 5);
        assert_eq!(host_tell(&mut ctx, 7).unwrap(), 3);
        assert_eq!(host_filetype(&mut ctx, 5).unwrap(), FILETYPE_DIRECTORY);
        assert!(host_filetype(&mut ctx, 6).is_err(), "6 is free, as it was");

        // The next file gets the same number in both.
        let how = How { dirflags: 0, oflags: OFLAGS_CREAT, rights_base: 0x1fffffff, rights_inheriting: 0, fdflags: 0 };
        let next_original = host_open(&mut original.wasi, 3, 0, "y.txt", OFLAGS_CREAT, &how).unwrap();
        let next_resumed = host_open(&mut ctx, 3, 0, "y.txt", OFLAGS_CREAT, &how).unwrap();
        assert_eq!((next_original, next_resumed), (6, 6));
    }

    #[test]
    fn a_changed_file_is_refused() {
        let dir = TestDir::new("wasi-changed");
        std::fs::create_dir(dir.0.join("sub")).unwrap();
        let (state, original) = run_files(&dir.0);
        drop(original);
        std::fs::write(dir.0.join("a.txt"), "hello, changed").unwrap();
        let e = restore(&mut context(&dir.0), &state, 1).unwrap_err().to_string();
        assert!(e.contains("a.txt changed since the state was saved (its size changed)"), "{e}");
    }

    #[test]
    fn the_clock_continues_and_stands_still_while_paused() {
        let clock = ProgramClock::new();
        clock.set_ns(5_000_000_000);
        let t0 = clock.now_ns();
        assert!((5_000_000_000..5_100_000_000).contains(&t0), "{t0}");
        clock.pause();
        let paused = clock.now_ns();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(clock.now_ns(), paused);
        clock.resume();
        std::thread::sleep(Duration::from_millis(10));
        let t1 = clock.now_ns();
        assert!(t1 >= paused + 10_000_000 && t1 < paused + 150_000_000, "{paused} -> {t1}");
    }
}
