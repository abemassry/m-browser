//! Saving QEMU: pausing it, checkpoints written by QEMU's own migration to
//! a file, and resuming them, several at once.
//!
//! m-browser cannot save QEMU's wasm state as it saves other programs
//! (saved_state/snapshot.rs): the multi-threaded QEMU's threads are always
//! inside wasm functions. QEMU can save itself, though: its migration
//! writes the guest's RAM and device state to a file (`migrate file:`), and
//! a new QEMU with the same arguments loads it (`migrate_incoming`). The
//! guest's disks are not in that stream, so each checkpoint freezes them:
//!
//! ```text
//! checkpoint (QEMU keeps running)          resume (any number of times, at once)
//!  stop                                     QEMU with the saved arguments, whose
//!  snapshot_blkdev <disk> <new overlay>       disks are the frozen images, and
//!    (the image so far is now frozen:         -incoming defer
//!     it belongs to the checkpoint)         snapshot_blkdev <disk> <new overlay>
//!  migrate -d file:<state>,offset=64K         (this copy writes to its overlay)
//!  ... steps go on until "completed"        migrate_incoming file:<state>,offset=64K
//!  the manifest goes into the first 64 KiB
//!  cont
//! ```
//!
//! The monitor commands reach QEMU through its `wasi_qemu_monitor` export
//! (`monitor`), between two `wasi_qemu_step` calls. Requests come from the
//! headless runner's FIFO or, later, the GUI (`QemuControl`); `Saver`
//! carries them out in `wasm::run_qemu_reactor`'s loop.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use wasmtime::{Instance, Store};

use crate::qemu_jit::{GuestMem, QemuHostState};
use crate::saved_state::wasi::FileIdentity;
use crate::saved_state::{self, Engine, Header, Launch, Mode, ModuleId, QemuLaunch, StateReader};

/// Where QEMU's migration stream starts in a saved state; m-browser's
/// manifest (header and sections) comes before it.
pub const STATE_OFFSET: u64 = 64 * 1024;

/// What can be asked of a running QEMU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Stop the guest (QEMU's `stop`); its clocks stand still.
    Pause,
    /// Let it run again (`cont`).
    Continue,
    /// Write a checkpoint; the guest goes on.
    Save,
    /// Run a monitor command and print its output.
    Monitor(String),
}

/// What a QEMU's `Saver` reports back, for the browser to act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// The checkpoint QEMU started from is loaded, and the guest runs if it
    /// ran when saved.
    Resumed,
    /// A checkpoint was written to this file.
    Saved(PathBuf),
    /// A request, a save or a resume failed; why.
    Failed(String),
}

/// Requests for a QEMU, from any thread, and its reports back.
#[derive(Clone, Default)]
pub struct QemuControl(Arc<Mutex<Exchange>>);

#[derive(Default)]
struct Exchange {
    requests: VecDeque<Request>,
    reports: Vec<Report>,
}

impl QemuControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn send(&self, request: Request) {
        self.0.lock().unwrap().requests.push_back(request);
    }

    fn next(&self) -> Option<Request> {
        self.0.lock().unwrap().requests.pop_front()
    }

    fn report(&self, report: Report) {
        self.0.lock().unwrap().reports.push(report);
    }

    /// The reports since the last call.
    pub fn reports(&self) -> Vec<Report> {
        std::mem::take(&mut self.0.lock().unwrap().reports)
    }
}

/// Run a command of QEMU's human monitor, as typed at its `(qemu)` prompt,
/// and return its output. Only between two steps.
pub fn monitor(store: &mut Store<QemuHostState>, instance: &Instance, command: &str) -> Result<String> {
    const MISSING: &str = "this QEMU build cannot take monitor commands from m-browser (it has no \
                           wasi_qemu_monitor export): install a newer one";
    let buffer = instance.get_typed_func::<(), i32>(&mut *store, "wasi_qemu_buffer").context(MISSING)?;
    let size = instance.get_typed_func::<(), i32>(&mut *store, "wasi_qemu_buffer_size").context(MISSING)?;
    let run = instance.get_typed_func::<i32, i32>(&mut *store, "wasi_qemu_monitor").context(MISSING)?;
    let at = buffer.call(&mut *store, ())? as u32 as usize;
    let size = size.call(&mut *store, ())? as u32 as usize;
    if command.len() >= size {
        bail!("monitor command too long: {command}");
    }
    write_guest(store, at, command.as_bytes())?;
    let length = run.call(&mut *store, command.len() as i32)?;
    if length < 0 {
        bail!("QEMU refused the monitor command {command:?}");
    }
    let output = read_guest(store, at, (length as usize).min(size))?;
    Ok(String::from_utf8_lossy(&output).into_owned())
}

/// A monitor command that prints nothing when it succeeds.
fn command(store: &mut Store<QemuHostState>, instance: &Instance, command: &str) -> Result<()> {
    let output = monitor(store, instance, command)?;
    if !output.trim().is_empty() {
        bail!("{command}: {}", output.trim());
    }
    Ok(())
}

fn write_guest(store: &mut Store<QemuHostState>, at: usize, bytes: &[u8]) -> Result<()> {
    match store.data().guest_memory.clone().context("QEMU's memory is not known yet")? {
        GuestMem::Plain(memory) => memory.write(&mut *store, at, bytes).context("monitor buffer out of bounds"),
        GuestMem::Shared(memory) => {
            let cells = memory.data().get(at..at + bytes.len()).context("monitor buffer out of bounds")?;
            // SAFETY: in bounds; QEMU's main thread is not running (this is
            // between two steps), and no other QEMU thread uses this buffer.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), cells.as_ptr() as *mut u8, bytes.len()) };
            Ok(())
        }
    }
}

fn read_guest(store: &Store<QemuHostState>, at: usize, length: usize) -> Result<Vec<u8>> {
    match store.data().guest_memory.clone().context("QEMU's memory is not known yet")? {
        GuestMem::Plain(memory) => {
            memory.data(store).get(at..at + length).map(<[u8]>::to_vec).context("monitor buffer out of bounds")
        }
        GuestMem::Shared(memory) => {
            let cells = memory.data().get(at..at + length).context("monitor buffer out of bounds")?;
            // SAFETY: as in write_guest.
            Ok(unsafe { std::slice::from_raw_parts(cells.as_ptr() as *const u8, length) }.to_vec())
        }
    }
}

/// A writable disk, as `info block` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Disk {
    device: String,
    /// The image QEMU writes, as QEMU names it (a path inside its sandbox).
    image: String,
    format: String,
}

/// The writable disks in the output of `info block`, whose entries start
/// with lines such as `ide0-hd0 (#block143): /data/m.qcow2 (qcow2)`.
fn parse_info_block(output: &str) -> Vec<Disk> {
    let mut disks = Vec::new();
    for line in output.lines() {
        if line.is_empty() || line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((head, rest)) = line.split_once(": ") else { continue };
        let device = head.split(' ').next().unwrap_or(head).to_string();
        let rest = rest.trim_end();
        if rest.starts_with('[') {
            continue; // [not inserted]
        }
        let Some(open) = rest.rfind(" (") else { continue };
        let details: Vec<&str> = rest[open + 2..].trim_end_matches(')').split(", ").collect();
        if details.contains(&"read-only") {
            continue;
        }
        disks.push(Disk { device, image: rest[..open].to_string(), format: details[0].to_string() });
    }
    disks
}

/// The options of a `-drive` argument, split at single commas (QEMU
/// writes a comma inside a value as `,,`).
fn drive_options(arg: &str) -> Vec<String> {
    let mut options = vec![String::new()];
    let mut chars = arg.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ',' {
            if chars.peek() == Some(&',') {
                chars.next();
                options.last_mut().unwrap().push_str(",,");
            } else {
                options.push(String::new());
            }
        } else {
            options.last_mut().unwrap().push(c);
        }
    }
    options
}

/// The file a `-drive` argument names, with its `,,` undone.
fn drive_file(arg: &str) -> Option<String> {
    drive_options(arg).iter().find_map(|o| o.strip_prefix("file=").map(|f| f.replace(",,", ",")))
}

/// A `-drive` argument with another file and format.
fn with_drive_file(arg: &str, file: &str, format: &str) -> String {
    let mut options: Vec<String> = drive_options(arg)
        .into_iter()
        .filter(|o| !o.starts_with("file=") && !o.starts_with("format="))
        .collect();
    options.insert(0, format!("format={format}"));
    options.insert(0, format!("file={}", file.replace(',', ",,")));
    options.join(",")
}

/// The argument index of the `-drive` whose file is `image`.
fn drive_index(args: &[String], image: &str) -> Option<usize> {
    (1..args.len()).find(|&i| args[i - 1] == "-drive" && drive_file(&args[i]).as_deref() == Some(image))
}

/// The host path of a path inside QEMU's sandbox.
fn host_path(guest: &str, preopens: &[(String, String)]) -> Option<PathBuf> {
    preopens
        .iter()
        .filter_map(|(host, prefix)| {
            let rest = guest.strip_prefix(prefix.trim_end_matches('/'))?;
            (rest.is_empty() || rest.starts_with('/')).then(|| (prefix.len(), Path::new(host).join(rest.trim_start_matches('/'))))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, path)| path)
}

/// The path inside QEMU's sandbox of a host path.
fn guest_path(host: &Path, preopens: &[(String, String)]) -> Option<String> {
    preopens
        .iter()
        .filter_map(|(dir, prefix)| {
            let rest = host.strip_prefix(dir).ok()?;
            let rest = rest.to_str()?;
            let prefix = prefix.trim_end_matches('/');
            Some((dir.len(), if rest.is_empty() { prefix.to_string() } else { format!("{prefix}/{rest}") }))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, path)| path)
}

/// An image and the backing files under it, as host paths: qcow2 images
/// name their backing file in their header; any other format ends the
/// chain.
fn image_chain(image: &Path, preopens: &[(String, String)]) -> Result<Vec<PathBuf>> {
    let mut chain = vec![image.to_path_buf()];
    while chain.len() < 64 {
        let current = chain.last().unwrap().clone();
        let mut file = std::fs::File::open(&current).with_context(|| format!("cannot open {}", current.display()))?;
        let mut header = [0u8; 20];
        if file.read_exact(&mut header).is_err() || header[..4] != *b"QFI\xfb" {
            break;
        }
        let offset = u64::from_be_bytes(header[8..16].try_into().unwrap());
        let size = u32::from_be_bytes(header[16..20].try_into().unwrap()) as usize;
        if offset == 0 || size == 0 || size > 4096 {
            break;
        }
        let mut name = vec![0u8; size];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut name)?;
        let name = String::from_utf8_lossy(&name).into_owned();
        let backing = if name.starts_with('/') {
            host_path(&name, preopens)
                .with_context(|| format!("{} is backed by {name}, outside QEMU's directories", current.display()))?
        } else {
            current.parent().unwrap_or(Path::new(".")).join(&name)
        };
        chain.push(backing);
    }
    Ok(chain)
}

/// A disk of a saved state: the image frozen with it, and the files under
/// that image, as they were when it was saved.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SavedDisk {
    pub device: String,
    /// The frozen image, as QEMU names it.
    pub image: String,
    pub format: String,
    pub chain: Vec<FileIdentity>,
}

/// The QEMU section of a saved state.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct QemuState {
    /// Where QEMU's migration stream is in the file, its length and CRC32.
    pub offset: u64,
    pub length: u64,
    pub crc32: u32,
    /// Whether the guest was running. QEMU's stream says it was paused
    /// (it is stopped while saved, so that its memory and its frozen
    /// disks are of the same moment): a resumed copy continues it.
    pub running: bool,
    pub disks: Vec<SavedDisk>,
}

/// What m-browser knows about a running QEMU, to save it.
pub struct Session {
    pub module: ModuleId,
    /// QEMU's arguments as they are now, argv[0] first: each disk's
    /// `-drive file=` is the image QEMU writes.
    pub args: Vec<String>,
    /// Preopened directories: (absolute host path, path QEMU sees).
    pub preopens: Vec<(String, String)>,
    /// Where checkpoints and overlays go (a host directory QEMU can reach).
    pub save_dir: PathBuf,
    /// The tab or headless session, and the checkpoints it made so far.
    pub owner: String,
    pub counter: u32,
}

impl Session {
    /// The directory checkpoints go to: `QEMU_SAVE_DIR`, else the working
    /// directory.
    pub fn default_save_dir() -> PathBuf {
        let dir = std::env::var_os("QEMU_SAVE_DIR").map_or_else(|| PathBuf::from("."), PathBuf::from);
        std::fs::canonicalize(&dir).unwrap_or(dir)
    }

    /// A name for new files of this session: a checkpoint, or the disk
    /// overlays that start with it.
    fn name(&self, counter: u32, created: u64) -> String {
        let name = saved_state::file_name(Path::new(&self.module.path), created, counter, &self.owner);
        name.trim_end_matches(&format!(".{}", saved_state::EXTENSION)).to_string()
    }

    fn save_dir_in_qemu(&self) -> Result<String> {
        guest_path(&self.save_dir, &self.preopens).with_context(|| {
            format!("{} is not a directory QEMU can reach (not preopened)", self.save_dir.display())
        })
    }

    /// Give QEMU's disks new overlays: from now on it writes to those, and
    /// the images so far are frozen. Updates the arguments to name the
    /// overlays, and returns the disks with their frozen images.
    fn switch_disks(&mut self, store: &mut Store<QemuHostState>, instance: &Instance, name: &str) -> Result<Vec<Disk>> {
        let disks = parse_info_block(&monitor(store, instance, "info block")?);
        let dir = self.save_dir_in_qemu()?;
        for (i, disk) in disks.iter().enumerate() {
            let arg = drive_index(&self.args, &disk.image).with_context(|| {
                format!("disk {} ({}) is not one of QEMU's -drive file= arguments", disk.device, disk.image)
            })?;
            let overlay = format!("{dir}/{name}.disk{i}.qcow2");
            command(store, instance, &format!("snapshot_blkdev {} {overlay} qcow2", disk.device))?;
            self.args[arg] = with_drive_file(&self.args[arg], &overlay, "qcow2");
        }
        Ok(disks)
    }
}

/// A checkpoint whose migration is under way.
struct Pending {
    tmp: PathBuf,
    path: PathBuf,
    counter: u32,
    created: u64,
    /// The arguments that start a copy on the frozen images.
    resume_args: Vec<String>,
    disks: Vec<Disk>,
    was_running: bool,
}

/// What `wasm::run_wasm_module` takes to let a QEMU be paused and saved.
pub struct Saving {
    pub control: QemuControl,
    /// A checkpoint to resume: QEMU's arguments then come from
    /// `Resume::args`.
    pub resume: Option<Resume>,
    /// Who the checkpoints and overlays belong to: in the GUI, the id of
    /// the tab (12 hex digits, see `saved_state::new_owner_id`); `None`
    /// makes a new id, as the headless runner does.
    pub owner: Option<String>,
}

/// Carries out the requests for a QEMU, between its steps.
pub struct Saver {
    session: Session,
    control: QemuControl,
    /// Whether this is the multi-threaded QEMU. Only it can save: QEMU's
    /// migration writes from a thread of its own, and the single-threaded
    /// build's threads never run (qemu-wasi's wasi-stubs/pthread_stubs.c).
    threaded: bool,
    pending: Option<Pending>,
    /// To do after `wasi_qemu_init`: load this checkpoint.
    resume: Option<Resume>,
    /// While resuming: the state file being loaded, and whether to let
    /// the guest run once it is.
    loading: Option<(PathBuf, bool)>,
}

impl Saver {
    /// A saver for a QEMU about to start from this module, with these
    /// arguments (argv[0] first, as QEMU gets them) and preopens.
    pub fn for_launch(module: &str, args: &[String], preopens: &[(&str, &str)], threaded: bool, saving: Saving) -> Result<Self> {
        let (module, _) = ModuleId::of_file(Path::new(module))?;
        let preopens = preopens
            .iter()
            .map(|(host, guest)| {
                let host = std::fs::canonicalize(host).with_context(|| format!("cannot open {host}"))?;
                Ok((host.to_string_lossy().into_owned(), guest.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let session = Session {
            module,
            args: without_incoming(args),
            preopens,
            save_dir: Session::default_save_dir(),
            owner: saving.owner.unwrap_or_else(saved_state::new_owner_id),
            counter: 0,
        };
        Ok(Saver { session, control: saving.control, threaded, pending: None, resume: saving.resume, loading: None })
    }

    /// Whether this QEMU is to resume a checkpoint rather than boot.
    pub fn is_resuming(&self) -> bool {
        self.resume.is_some()
    }

    /// Call right after `wasi_qemu_init`: starts loading the checkpoint,
    /// if there is one to resume.
    pub fn after_init(&mut self, store: &mut Store<QemuHostState>, instance: &Instance) -> Result<()> {
        match self.resume.take() {
            Some(resume) => self.start_resume(store, instance, &resume),
            None => Ok(()),
        }
    }

    /// Handle what is asked; call between two steps. Errors are reported,
    /// not returned: QEMU goes on.
    pub fn between_steps(&mut self, store: &mut Store<QemuHostState>, instance: &Instance) {
        if self.loading.is_some() || self.pending.is_some() {
            if let Err(e) = self.poll(store, instance) {
                eprintln!("[qemu] {e:#}");
                self.control.report(Report::Failed(format!("{e:#}")));
            }
            return;
        }
        while let Some(request) = self.control.next() {
            let result = match &request {
                Request::Pause => command(store, instance, "stop"),
                Request::Continue => command(store, instance, "cont"),
                Request::Monitor(text) => monitor(store, instance, text).map(|output| {
                    eprintln!("[qemu] (qemu) {text}\n{}", output.trim_end());
                }),
                Request::Save => self.start_save(store, instance),
            };
            if let Err(e) = result {
                eprintln!("[qemu] {request:?} failed: {e:#}");
                self.control.report(Report::Failed(format!("{e:#}")));
            }
            if self.pending.is_some() {
                return;
            }
        }
    }

    fn start_save(&mut self, store: &mut Store<QemuHostState>, instance: &Instance) -> Result<()> {
        if !self.threaded {
            bail!(
                "only the multi-threaded QEMU can save (QEMU writes its state from a thread of its own); \
                 this is the single-threaded build"
            );
        }
        let was_running = monitor(store, instance, "info status")?.contains("running");
        if was_running {
            command(store, instance, "stop")?;
        }
        let result = self.freeze_and_migrate(store, instance, was_running);
        if result.is_err() && was_running {
            let _ = command(store, instance, "cont");
        }
        result
    }

    fn freeze_and_migrate(&mut self, store: &mut Store<QemuHostState>, instance: &Instance, was_running: bool) -> Result<()> {
        let counter = self.session.counter + 1;
        let created = saved_state::unix_now();
        let name = self.session.name(counter, created);
        let resume_args_before = self.session.args.clone();
        let disks = self.session.switch_disks(store, instance, &name)?;
        // The copy that resumes starts on the images just frozen.
        let mut resume_args = resume_args_before;
        for disk in &disks {
            if let Some(arg) = drive_index(&resume_args, &disk.image) {
                resume_args[arg] = with_drive_file(&resume_args[arg], &disk.image, &disk.format);
            }
        }
        let file = format!("{name}.{}", saved_state::EXTENSION);
        let tmp = self.session.save_dir.join(format!("{file}.tmp"));
        let tmp_in_qemu = format!("{}/{file}.tmp", self.session.save_dir_in_qemu()?);
        command(store, instance, &format!("migrate -d file:{tmp_in_qemu},offset={STATE_OFFSET}"))?;
        eprintln!("[qemu] saving a checkpoint: the guest's disks are frozen, its memory is being written");
        self.pending = Some(Pending {
            tmp,
            path: self.session.save_dir.join(file),
            counter,
            created,
            resume_args,
            disks,
            was_running,
        });
        Ok(())
    }

    /// Follow a migration in either direction.
    fn poll(&mut self, store: &mut Store<QemuHostState>, instance: &Instance) -> Result<()> {
        let output = monitor(store, instance, "info migrate")?;
        let status = output
            .lines()
            .find_map(|l| l.strip_prefix("Migration status: "))
            .unwrap_or("")
            .trim()
            .to_string();
        if let Some((path, running)) = self.loading.clone() {
            if status == "completed" {
                self.loading = None;
                if running {
                    command(store, instance, "cont")?;
                }
                eprintln!("[qemu] resumed from {}", path.display());
                self.control.report(Report::Resumed);
            } else if status == "failed" {
                self.loading = None;
                bail!("loading {} failed:\n{}", path.display(), output.trim());
            }
            return Ok(());
        }
        match status.as_str() {
            "completed" => {
                let pending = self.pending.take().expect("polled while pending");
                let was_running = pending.was_running;
                let result = self.finish(pending);
                if was_running {
                    command(store, instance, "cont")?;
                }
                let path = result?;
                eprintln!("[qemu] saved a checkpoint to {}", path.display());
                self.control.report(Report::Saved(path));
                Ok(())
            }
            "failed" | "cancelled" => {
                let pending = self.pending.take().expect("polled while pending");
                let _ = std::fs::remove_file(&pending.tmp);
                if pending.was_running {
                    command(store, instance, "cont")?;
                }
                bail!("saving failed; QEMU said:\n{}", output.trim());
            }
            _ => Ok(()),
        }
    }

    /// The migration is complete: write the manifest in front of QEMU's
    /// stream and give the file its name.
    fn finish(&mut self, pending: Pending) -> Result<PathBuf> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pending.tmp)
            .with_context(|| format!("cannot open {}", pending.tmp.display()))?;
        let length = file.metadata()?.len().checked_sub(STATE_OFFSET).context("QEMU wrote no state")?;
        file.seek(SeekFrom::Start(STATE_OFFSET))?;
        let mut crc = crc32fast::Hasher::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
        }
        let mut disks = Vec::new();
        for disk in &pending.disks {
            let host = host_path(&disk.image, &self.session.preopens)
                .with_context(|| format!("{} is outside QEMU's directories", disk.image))?;
            let chain = image_chain(&host, &self.session.preopens)?
                .iter()
                .map(|p| FileIdentity::of(p))
                .collect::<Result<Vec<_>>>()?;
            disks.push(SavedDisk { device: disk.device.clone(), image: disk.image.clone(), format: disk.format.clone(), chain });
        }
        let state = QemuState { offset: STATE_OFFSET, length, crc32: crc.finalize(), running: pending.was_running, disks };
        let header = Header {
            engine: Engine::QemuMigration,
            mode: Mode::Checkpoint,
            module: self.session.module.clone(),
            launch: Launch::Qemu(QemuLaunch { args: pending.resume_args, preopens: self.session.preopens.clone() }),
            m_browser_version: env!("CARGO_PKG_VERSION").to_string(),
            created_unix_secs: pending.created,
            owner: self.session.owner.clone(),
            counter: pending.counter,
        };
        let manifest = saved_state::encode(&header, &[(saved_state::TAG_QEMU, postcard::to_allocvec(&state)?)])?;
        if manifest.len() as u64 > STATE_OFFSET {
            bail!("the manifest ({} bytes) does not fit before QEMU's state", manifest.len());
        }
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&manifest)?;
        file.sync_all()?;
        std::fs::rename(&pending.tmp, &pending.path)?;
        self.session.counter = pending.counter;
        Ok(pending.path)
    }
}

impl Drop for Saver {
    /// QEMU stopped during a save: the checkpoint will never be complete.
    fn drop(&mut self) {
        if let Some(pending) = &self.pending {
            let _ = std::fs::remove_file(&pending.tmp);
        }
    }
}

/// A checkpoint to resume: how to start QEMU, and what to do once it runs.
pub struct Resume {
    pub path: PathBuf,
    pub module: PathBuf,
    /// QEMU's arguments, ending with `-incoming defer`.
    pub args: Vec<String>,
    pub preopens: Vec<(String, String)>,
    state: QemuState,
}

/// Read and check a QEMU checkpoint: its manifest, its module, its state
/// stream and its frozen disk images.
pub fn prepare_resume(path: &Path) -> Result<Resume> {
    let path = std::fs::canonicalize(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = StateReader::open(&path)?;
    let header = reader.header.clone();
    let Launch::Qemu(launch) = header.launch else {
        bail!("{} is not a QEMU checkpoint: resume it with m-browser --wasm-headless --resume", path.display());
    };
    let mut state = None;
    while let Some(mut section) = reader.next()? {
        if section.tag == saved_state::TAG_QEMU {
            state = Some(postcard::from_bytes::<QemuState>(&section.read_rest()?)?);
        }
        section.end()?;
    }
    let state = state.with_context(|| format!("{} has no QEMU section", path.display()))?;
    let (module, _) = ModuleId::of_file(Path::new(&header.module.path))
        .with_context(|| format!("cannot read {}, the QEMU the checkpoint was saved from", header.module.path))?;
    if module.sha256 != header.module.sha256 {
        bail!(
            "{} has changed since the checkpoint was saved (SHA-256 {} then, {} now); a checkpoint resumes only \
             with the QEMU that saved it",
            module.path,
            header.module.sha256_hex(),
            module.sha256_hex()
        );
    }
    for disk in &state.disks {
        for saved in &disk.chain {
            let now = FileIdentity::of(Path::new(&saved.host_path))?;
            if let Some(change) = saved.change(&now) {
                bail!(
                    "{} changed since the checkpoint was saved ({change}); its disk would not match its memory",
                    saved.host_path
                );
            }
        }
    }
    let mut file = std::fs::File::open(&path)?;
    if file.metadata()?.len() != state.offset + state.length {
        bail!("{} is truncated", path.display());
    }
    file.seek(SeekFrom::Start(state.offset))?;
    let mut crc = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        crc.update(&buf[..n]);
    }
    if crc.finalize() != state.crc32 {
        bail!("{} is damaged (QEMU's state fails its check)", path.display());
    }
    let mut args = launch.args;
    args.extend(["-incoming".to_string(), "defer".to_string()]);
    Ok(Resume { path, module: PathBuf::from(&header.module.path), args, preopens: launch.preopens, state })
}

impl Saver {
    /// Right after `wasi_qemu_init` of a QEMU started with a `Resume`'s
    /// arguments: give its disks overlays of their own, then load the
    /// state. The guest starts once it is loaded.
    fn start_resume(&mut self, store: &mut Store<QemuHostState>, instance: &Instance, resume: &Resume) -> Result<()> {
        let name = self.session.name(0, saved_state::unix_now());
        self.session.switch_disks(store, instance, &name)?;
        let state = guest_path(&resume.path, &self.session.preopens)
            .with_context(|| format!("{} is not in a directory QEMU can reach", resume.path.display()))?;
        command(store, instance, &format!("migrate_incoming file:{state},offset={}", resume.state.offset))?;
        self.loading = Some((resume.path.clone(), resume.state.running));
        Ok(())
    }
}

/// The host paths of a checkpoint's disk images: each disk's frozen image
/// and the files under it. Only reads the manifest.
pub fn checkpoint_images(path: &Path) -> Result<Vec<PathBuf>> {
    let mut reader = StateReader::open(path)?;
    let mut images = Vec::new();
    while let Some(mut section) = reader.next()? {
        if section.tag == saved_state::TAG_QEMU {
            let state = postcard::from_bytes::<QemuState>(&section.read_rest()?)?;
            images.extend(state.disks.iter().flat_map(|d| d.chain.iter().map(|f| PathBuf::from(&f.host_path))));
        }
        section.end()?;
    }
    Ok(images)
}

/// Remove `-incoming defer` from QEMU's arguments.
pub fn without_incoming(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-incoming" {
            i += 2;
            continue;
        }
        out.push(args[i].clone());
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saved_state::tests::TestDir;

    #[test]
    fn info_block_gives_the_writable_disks() {
        let output = "ide0-hd0 (#block143): /data/m.qcow2 (qcow2)\n    Attached to:      /machine/unattached/device[23]\n    \
                      Cache mode:       writeback\n    Backing file:     /out/base.raw (chain depth: 1)\n\n\
                      ide1-cd0: [not inserted]\n    Attached to:      /machine/unattached/device[24]\n\n\
                      floppy0: [not inserted]\n\nvirtio1 (#block5): /out/ro.img (raw, read-only)\n";
        assert_eq!(
            parse_info_block(output),
            [Disk { device: "ide0-hd0".into(), image: "/data/m.qcow2".into(), format: "qcow2".into() }]
        );
    }

    #[test]
    fn drive_arguments_are_rewritten() {
        let args: Vec<String> =
            ["qemu", "-m", "1024", "-drive", "file=/data/a,,b.raw,format=raw,if=virtio"].map(String::from).to_vec();
        assert_eq!(drive_file(&args[4]).as_deref(), Some("/data/a,b.raw"));
        assert_eq!(drive_index(&args, "/data/a,b.raw"), Some(4));
        assert_eq!(drive_index(&args, "/data/m.qcow2"), None);
        assert_eq!(with_drive_file(&args[4], "/out/x.qcow2", "qcow2"), "file=/out/x.qcow2,format=qcow2,if=virtio");
        let incoming: Vec<String> = ["qemu", "-incoming", "defer", "-m", "1"].map(String::from).to_vec();
        assert_eq!(without_incoming(&incoming), ["qemu", "-m", "1"]);
    }

    #[test]
    fn paths_map_between_host_and_qemu() {
        let preopens = vec![("/home/u/mb".to_string(), "/data".to_string()), ("/tmp/w".to_string(), "/out".to_string())];
        assert_eq!(host_path("/data/m.qcow2", &preopens), Some(PathBuf::from("/home/u/mb/m.qcow2")));
        assert_eq!(host_path("/database", &preopens), None);
        assert_eq!(guest_path(Path::new("/tmp/w/k.qcow2"), &preopens).as_deref(), Some("/out/k.qcow2"));
        assert_eq!(guest_path(Path::new("/tmp/w"), &preopens).as_deref(), Some("/out"));
        assert_eq!(guest_path(Path::new("/etc"), &preopens), None);
    }

    #[test]
    fn a_checkpoint_is_checked_before_resuming() {
        let dir = TestDir::new("qemu-resume");
        let module = dir.0.join("qemu.wasm");
        std::fs::write(&module, b"\0asm\x01\0\0\0").unwrap();
        let disk = dir.0.join("frozen.qcow2");
        std::fs::write(&disk, [7u8; 4096]).unwrap();
        let stream = b"QEMU's migration stream".to_vec();
        let write = |path: &Path, launch: Launch, stream: &[u8]| {
            let state = QemuState {
                offset: STATE_OFFSET,
                length: stream.len() as u64,
                crc32: crc32fast::hash(stream),
                running: true,
                disks: vec![SavedDisk {
                    device: "ide0-hd0".into(),
                    image: "/out/frozen.qcow2".into(),
                    format: "qcow2".into(),
                    chain: vec![FileIdentity::of(&disk).unwrap()],
                }],
            };
            let header = Header {
                engine: Engine::QemuMigration,
                mode: Mode::Checkpoint,
                module: ModuleId::of_file(&module).unwrap().0,
                launch,
                ..crate::saved_state::tests::header()
            };
            let mut file =
                saved_state::encode(&header, &[(saved_state::TAG_QEMU, postcard::to_allocvec(&state).unwrap())]).unwrap();
            file.resize(STATE_OFFSET as usize, 0);
            file.extend_from_slice(stream);
            std::fs::write(path, file).unwrap();
        };
        let launch = Launch::Qemu(QemuLaunch {
            args: vec!["qemu".into(), "-drive".into(), "file=/out/frozen.qcow2,format=qcow2".into()],
            preopens: vec![(dir.0.to_string_lossy().into_owned(), "/out".into())],
        });
        let path = dir.0.join("a.wasmsavestate");
        write(&path, launch.clone(), &stream);
        let resume = prepare_resume(&path).unwrap();
        assert_eq!(resume.args[1..], ["-drive", "file=/out/frozen.qcow2,format=qcow2", "-incoming", "defer"]);
        assert!(resume.state.running);
        assert_eq!(checkpoint_images(&path).unwrap(), [disk.clone()]);

        let error = |path: &Path| format!("{:#}", prepare_resume(path).err().expect("refused"));
        let damaged = dir.0.join("damaged.wasmsavestate");
        let mut bytes = std::fs::read(&path).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        std::fs::write(&damaged, bytes).unwrap();
        assert!(error(&damaged).contains("is damaged"), "{}", error(&damaged));

        let core = dir.0.join("core.wasmsavestate");
        write(&core, crate::saved_state::tests::header().launch, &stream);
        assert!(error(&core).contains("not a QEMU checkpoint"), "{}", error(&core));

        std::fs::write(&disk, [8u8; 8192]).unwrap();
        assert!(error(&path).contains("its disk would not match"), "{}", error(&path));
        write(&path, launch, &stream);
        std::fs::write(&module, b"\0asm\x01\0\0\0\0").unwrap();
        assert!(error(&path).contains("has changed since the checkpoint"), "{}", error(&path));
    }

    #[test]
    fn backing_chains_are_followed() {
        let dir = TestDir::new("qemu-chain");
        let host = dir.0.to_string_lossy().into_owned();
        let preopens = vec![(host.clone(), "/out".to_string())];
        std::fs::write(dir.0.join("base.raw"), [0u8; 512]).unwrap();
        // The start of a qcow2 header: magic, version, backing file offset and size.
        let qcow2 = |backing: &str| {
            let mut h = b"QFI\xfb".to_vec();
            h.extend_from_slice(&3u32.to_be_bytes());
            h.extend_from_slice(&64u64.to_be_bytes());
            h.extend_from_slice(&(backing.len() as u32).to_be_bytes());
            h.resize(64, 0);
            h.extend_from_slice(backing.as_bytes());
            h
        };
        std::fs::write(dir.0.join("mid.qcow2"), qcow2("/out/base.raw")).unwrap();
        std::fs::write(dir.0.join("top.qcow2"), qcow2("mid.qcow2")).unwrap();
        let chain = image_chain(&dir.0.join("top.qcow2"), &preopens).unwrap();
        let names: Vec<String> = chain.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["top.qcow2", "mid.qcow2", "base.raw"]);
    }
}
