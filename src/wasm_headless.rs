//! `--wasm-headless`: run a core wasm module that is not QEMU, without a
//! window, and pause it, hibernate it to a file, and resume it from there.
//! The GUI runs such modules with the same code (`start`, `resume`) in a
//! tab, with their output on a text page (`OutputPage`) instead of
//! m-browser's own standard output; see tab_program.rs.
//!
//! Usage (from any directory):
//!   m-browser --wasm-headless [--preopen HOST=GUEST]... [--env KEY=VALUE]...
//!       [--step EXPORT] <module.wasm> [args...]
//!   m-browser --wasm-headless --resume <file.wasmsavestate>
//!
//! The module gets WASI preview 1: its arguments (argv[0] is the module's
//! file name), the given environment variables and directories, and
//! m-browser's standard input and output.
//!
//! - Without `--step` it is a command: `_start` runs to the end.
//! - With `--step EXPORT` it is a reactor: `_initialize` if it exports one,
//!   then EXPORT (no parameters, an i32 result) in a loop until it returns
//!   0 or more, its exit status (QEMU's `wasi_qemu_step` works this way).
//!
//! Environment:
//!   WASM_CONTROL_FIFO=<path>  lines read from this named pipe: pause,
//!                             continue, hibernate, quit
//!   WASM_HIBERNATE_AFTER=<s>  hibernate at the first save point after s
//!                             seconds
//!
//! Hibernating writes `<module>-<UTC time>-<n>-<session>.wasmsavestate`
//! into the working directory and ends the run; `--resume` continues it
//! from there, in the same session. A program can be saved only at a
//! *save point*, when none of its functions is running: between two steps
//! of a reactor, or, in a command transformed with Binaryen's Asyncify, at
//! a call to one of the WASI functions saved_state::wasi wraps (the
//! program then unwinds its stack into its memory). Other commands can be
//! paused at those calls but not saved. See saved_state.rs.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use wasmtime::{Caller, Config, Instance, Linker, Module, Store, Val};
use wasmtime_wasi::p1::WasiP1Ctx;

use crate::saved_state::snapshot::{self, AsyncifySaved, Layout};
use crate::saved_state::wasi::{self as saved_wasi, FileJournal, ProgramClock, WasiHost, WasiState};
use crate::saved_state::{self, CoreLaunch, Entry, Header, Launch, Mode, ModuleId, StateReader, StateWriter};

const USAGE: &str = "usage: m-browser --wasm-headless [--preopen HOST=GUEST]... [--env KEY=VALUE]... \
                     [--step EXPORT] <module.wasm> [args...]\n       \
                     m-browser --wasm-headless --resume <file.wasmsavestate>";

/// Run `--wasm-headless` with these arguments; returns m-browser's exit
/// status.
pub fn run(args: &[String]) -> Result<i32> {
    let control = Control::new();
    if let Some(path) = std::env::var_os("WASM_CONTROL_FIFO") {
        spawn_control_fifo(path.into(), control.clone());
    }
    if let Ok(secs) = std::env::var("WASM_HIBERNATE_AFTER") {
        let secs = secs
            .parse::<f64>()
            .ok()
            .filter(|s| s.is_finite() && *s >= 0.0)
            .context("WASM_HIBERNATE_AFTER must be a number of seconds")?;
        let control = control.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f64(secs));
            eprintln!("[wasm] WASM_HIBERNATE_AFTER: hibernating at the next save point");
            control.send(Command::Hibernate);
        });
    }
    let save_dir = std::env::current_dir()?;
    let outcome = match args {
        [flag, path] if flag == "--resume" => resume(Path::new(path), &save_dir, control, None)?,
        [flag, ..] if flag == "--resume" => bail!("{USAGE}"),
        _ => start(&parse_options(args, save_dir)?, control)?,
    };
    Ok(match outcome {
        Outcome::Exited(status) => {
            eprintln!("[wasm] exited with status {status}");
            status
        }
        Outcome::Hibernated(path) => {
            eprintln!("[wasm] hibernated to {}", path.display());
            0
        }
        Outcome::Quit => {
            eprintln!("[wasm] stopped by a quit request");
            0
        }
    })
}

/// How to start a module.
pub struct Options {
    pub module: PathBuf,
    /// Its arguments, after argv[0].
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// (host directory, the path the program sees).
    pub preopens: Vec<(String, String)>,
    /// For a reactor, the export to call in a loop.
    pub step: Option<String>,
    /// Where hibernated states are written.
    pub save_dir: PathBuf,
    /// Where its standard output and error go: a page the browser shows,
    /// or (`None`) m-browser's own.
    pub output: Option<OutputPage>,
    /// Who its saved states belong to: in the GUI, the tab's id; `None`
    /// makes a new id.
    pub owner: Option<String>,
}

fn parse_options(args: &[String], save_dir: PathBuf) -> Result<Options> {
    let (mut preopens, mut env, mut step) = (Vec::new(), Vec::new(), None);
    let mut rest = args;
    while let Some(flag) = rest.first() {
        let value = || rest.get(1).with_context(|| format!("{flag} needs a value\n{USAGE}"));
        match flag.as_str() {
            "--preopen" => {
                let spec = value()?;
                let (host, guest) =
                    spec.split_once('=').with_context(|| format!("--preopen expects HOST=GUEST, got {spec:?}"))?;
                preopens.push((host.to_string(), guest.to_string()));
            }
            "--env" => {
                let spec = value()?;
                let (key, val) =
                    spec.split_once('=').with_context(|| format!("--env expects KEY=VALUE, got {spec:?}"))?;
                env.push((key.to_string(), val.to_string()));
            }
            "--step" => step = Some(value()?.clone()),
            _ => break,
        }
        rest = &rest[2..];
    }
    let Some((module, args)) = rest.split_first() else { bail!("{USAGE}") };
    Ok(Options { module: module.into(), args: args.to_vec(), env, preopens, step, save_dir, output: None, owner: None })
}

/// How a run ended.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// The program ended with this status.
    Exited(i32),
    /// The program was saved to this file and stopped.
    Hibernated(PathBuf),
    /// Stopped by a quit request, without saving.
    Quit,
}

/// Start a module.
pub fn start(options: &Options, control: Arc<Control>) -> Result<Outcome> {
    let (module, wasm) = ModuleId::of_file(&options.module)?;
    let argv0 = options.module.file_name().map_or_else(|| "module".into(), |n| n.to_string_lossy().into_owned());
    let mut preopens = Vec::with_capacity(options.preopens.len());
    for (host, guest) in &options.preopens {
        // Absolute, so that a resume works from any directory.
        let host = std::fs::canonicalize(host).with_context(|| format!("cannot preopen {host}"))?;
        preopens.push((host.to_string_lossy().into_owned(), guest.clone()));
    }
    let launch = CoreLaunch {
        args: std::iter::once(argv0).chain(options.args.iter().cloned()).collect(),
        env: options.env.clone(),
        preopens,
        entry: options.step.clone().map_or(Entry::Command, |step| Entry::Reactor { step }),
    };
    let owner = options.owner.clone().unwrap_or_else(saved_state::new_owner_id);
    let output = options.output.as_ref();
    let mut program = Program::new(&wasm, module, launch, owner, 0, options.save_dir.clone(), control.clone(), output)?;
    control.set_can_hibernate(&program);
    if let Some(problem) = program.layout.problems.first() {
        note(output, &format!("note: this program cannot be saved: {problem}"));
    } else if program.launch.entry == Entry::Command && !program.layout.asyncify {
        note(output, &format!("note: {NO_SAVE_POINTS}"));
    }
    program.run(None)
}

/// A message of m-browser's about a program: on standard error, and on the
/// program's page if it has one.
fn note(output: Option<&OutputPage>, text: &str) {
    eprintln!("[wasm] {text}");
    if let Some(page) = output {
        page.note(text);
    }
}

const NO_SAVE_POINTS: &str = "this command can be paused but not saved: it has no save points \
                              (transform it with `wasm-opt --asyncify`, or run a reactor with --step)";

/// Resume a program from a saved state; `save_dir` is where it hibernates
/// next, and `output` where its output goes (`None`: m-browser's own).
pub fn resume(path: &Path, save_dir: &Path, control: Arc<Control>, output: Option<OutputPage>) -> Result<Outcome> {
    let mut reader = StateReader::open(path)?;
    let header = reader.header.clone();
    if header.engine != saved_state::Engine::WasmSnapshot {
        bail!("{} was not saved by the generic engine", path.display());
    }
    let Launch::Core(launch) = header.launch else {
        bail!("{} holds QEMU: resume it with m-browser --qemu-headless --resume", path.display());
    };
    let (module, wasm) = ModuleId::of_file(Path::new(&header.module.path))
        .with_context(|| format!("cannot read {}, the module the state was saved from", header.module.path))?;
    if module.sha256 != header.module.sha256 {
        bail!(
            "{} has changed since the state was saved (SHA-256 {} then, {} now); a state resumes only with \
             the module it was saved from",
            module.path,
            header.module.sha256_hex(),
            module.sha256_hex()
        );
    }
    let preopens = launch.preopens.len();
    let mut program = Program::new(
        &wasm,
        module,
        launch,
        header.owner,
        header.counter,
        save_dir.into(),
        control.clone(),
        output.as_ref(),
    )?;
    control.set_can_hibernate(&program);
    let mut restore = snapshot::Restore::new(&program.layout);
    let (mut wasi, mut asyncify) = (None, None);
    while let Some(mut section) = reader.next()? {
        match section.tag {
            saved_state::TAG_WASI => wasi = Some(postcard::from_bytes::<WasiState>(&section.read_rest()?)?),
            saved_state::TAG_ASYNCIFY => {
                asyncify = Some(postcard::from_bytes::<AsyncifySaved>(&section.read_rest()?)?);
            }
            _ => {
                if !restore.section(&mut program.store, &program.instance, &mut section)? {
                    eprintln!("[wasm] ignoring section {} of {}", section.name(), path.display());
                }
            }
        }
        section.end()?;
    }
    restore.finish()?;
    let wasi = wasi.context("the saved state has no WASI section")?;
    let data = program.store.data_mut();
    data.clock.set_ns(wasi.monotonic_ns);
    data.journal = saved_wasi::restore(&mut data.wasi, &wasi, preopens)?;
    if program.launch.entry == Entry::Command && asyncify.is_none() {
        bail!("{} is damaged: it holds a command without its call stack", path.display());
    }
    note(output.as_ref(), &format!("resuming from {}", path.display()));
    program.run(Some(asyncify))
}

/// Requests to a running program: from the control FIFO, a timer, or a
/// test.
pub struct Control {
    state: Mutex<Requests>,
    changed: Condvar,
    /// Whether the program can be hibernated: it has save points and
    /// nothing that cannot be saved. Set by `start` and `resume`.
    can_hibernate: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct Requests {
    paused: bool,
    hibernate: bool,
    quit: bool,
    /// Tests: hibernate at this save point (1 is the first).
    hibernate_at: Option<u64>,
    save_points: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Pause,
    Continue,
    Hibernate,
    Quit,
}

impl Command {
    fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "pause" => Some(Command::Pause),
            "continue" => Some(Command::Continue),
            "hibernate" => Some(Command::Hibernate),
            "quit" => Some(Command::Quit),
            _ => None,
        }
    }
}

/// Where the program is when it asks what to do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Point {
    /// It may be paused and saved here.
    Save,
    /// It may be paused here, not saved (a command without Asyncify).
    Pause,
    /// Neither: inside a reactor's step, which pauses between steps.
    Pass,
}

enum Action {
    Run,
    Hibernate,
    Quit,
}

impl Control {
    pub fn new() -> Arc<Self> {
        Arc::new(Control {
            state: Mutex::new(Requests::default()),
            changed: Condvar::new(),
            can_hibernate: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Whether a hibernate request can succeed (known once the program has
    /// been loaded).
    pub fn can_hibernate(&self) -> bool {
        self.can_hibernate.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn set_can_hibernate(&self, program: &Program) {
        let savable = program.layout.problems.is_empty()
            && (program.layout.asyncify || matches!(program.launch.entry, Entry::Reactor { .. }));
        self.can_hibernate.store(savable, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn send(&self, command: Command) {
        let mut state = self.state.lock().unwrap();
        match command {
            Command::Pause => state.paused = true,
            Command::Continue => state.paused = false,
            Command::Hibernate => state.hibernate = true,
            Command::Quit => state.quit = true,
        }
        self.changed.notify_all();
    }

    /// What to do at this point. While the program is paused this waits,
    /// and the program's clock stands still.
    fn at(&self, clock: &ProgramClock, point: Point) -> Action {
        let mut state = self.state.lock().unwrap();
        if point == Point::Save {
            state.save_points += 1;
            if state.hibernate_at == Some(state.save_points) {
                state.hibernate = true;
            }
        }
        let decline = |s: &mut Requests| {
            if point == Point::Pause && s.hibernate {
                s.hibernate = false;
                eprintln!("[wasm] cannot hibernate: {NO_SAVE_POINTS}");
            }
        };
        decline(&mut state);
        let waits = |s: &Requests| s.paused && !s.quit && !(point == Point::Save && s.hibernate);
        if point != Point::Pass && waits(&state) {
            eprintln!("[wasm] paused");
            clock.pause();
            while waits(&state) {
                state = self.changed.wait(state).unwrap();
                decline(&mut state);
            }
            clock.resume();
            if !state.quit {
                eprintln!("[wasm] continuing");
            }
        }
        if state.quit {
            Action::Quit
        } else if point == Point::Save && state.hibernate {
            state.hibernate = false;
            Action::Hibernate
        } else {
            Action::Run
        }
    }
}

/// Read commands from a named pipe; reopen it after each writer.
fn spawn_control_fifo(path: PathBuf, control: Arc<Control>) {
    std::thread::spawn(move || loop {
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) => {
                eprintln!("[wasm] cannot open {}: {e}", path.display());
                return;
            }
        };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            match Command::parse(&line) {
                Some(command) => {
                    eprintln!("[wasm] {}", line.trim());
                    control.send(command);
                }
                None if line.trim().is_empty() => {}
                None => eprintln!("[wasm] unknown command {line:?}: pause, continue, hibernate or quit"),
            }
        }
    });
}

/// A quit request, raised from inside the program as a trap.
#[derive(Debug)]
struct Quit;

impl std::fmt::Display for Quit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stopped by a quit request")
    }
}

impl std::error::Error for Quit {}

/// The Store's data.
struct RunnerState {
    wasi: WasiP1Ctx,
    journal: FileJournal,
    clock: ProgramClock,
    control: Arc<Control>,
    /// Where a wrapped WASI call is in the program's life (see `Point`).
    point: Point,
    /// Asyncify: the buffer for the unwound stack, the call the program
    /// unwound from, and, while resuming, the call it must arrive at.
    buffer: Option<u32>,
    unwound_at: Option<&'static str>,
    rewind_to: Option<String>,
}

impl WasiHost for RunnerState {
    fn wasi(&mut self) -> &mut WasiP1Ctx {
        &mut self.wasi
    }

    fn journal(&mut self) -> &mut FileJournal {
        &mut self.journal
    }
}

/// Before every wrapped WASI call: finish a rewind, wait while paused, or
/// unwind the stack to hibernate.
fn save_point(caller: &mut Caller<'_, RunnerState>, call: &'static str) -> Result<Option<i32>> {
    if let Some(expected) = caller.data_mut().rewind_to.take() {
        // Resuming: the program is back in the call it was saved in, with
        // its stack rebuilt. Now the call really runs.
        if expected != call {
            bail!("cannot resume: the program was saved in {expected}, but its stack came back in {call}");
        }
        call_export(caller, "asyncify_stop_rewind", &[])?;
        return Ok(None);
    }
    let (control, clock, point) = {
        let data = caller.data();
        (data.control.clone(), data.clock.clone(), data.point)
    };
    match control.at(&clock, point) {
        Action::Run => Ok(None),
        Action::Quit => Err(Quit.into()),
        Action::Hibernate => {
            if let Some(problem) = caller.data().journal.problem() {
                eprintln!("[wasm] cannot hibernate: its open files cannot be saved: {problem}");
                return Ok(None);
            }
            unwind(caller, call)?;
            // Ignored: the program unwinds instead of looking at it.
            Ok(Some(0))
        }
    }
}

/// Make an Asyncify program copy its stack into its memory and return.
fn unwind(caller: &mut Caller<'_, RunnerState>, call: &'static str) -> Result<()> {
    let memory = caller
        .get_export(&snapshot::memory_export(0))
        .and_then(|e| e.into_memory())
        .context("Asyncify keeps the stack in memory 0, which this module does not define")?;
    let buffer = match caller.data().buffer {
        Some(buffer) => buffer,
        None => snapshot::asyncify_buffer(&mut *caller, memory)?,
    };
    snapshot::asyncify_reset(&mut *caller, memory, buffer)?;
    let data = caller.data_mut();
    data.buffer = Some(buffer);
    data.unwound_at = Some(call);
    call_export(caller, "asyncify_start_unwind", &[Val::I32(buffer as i32)])
}

fn call_export(caller: &mut Caller<'_, RunnerState>, name: &str, args: &[Val]) -> Result<()> {
    let func = caller
        .get_export(name)
        .and_then(|e| e.into_func())
        .with_context(|| format!("the module has no export {name}"))?;
    func.call(&mut *caller, args, &mut [])
}

/// A program and everything needed to run, save and resume it.
struct Program {
    store: Store<RunnerState>,
    instance: Instance,
    layout: Layout,
    module: ModuleId,
    launch: CoreLaunch,
    owner: String,
    /// Saves made in this session so far.
    counter: u32,
    save_dir: PathBuf,
}

impl Program {
    /// Prepare and instantiate the module, without running any of it.
    fn new(
        wasm: &[u8],
        module: ModuleId,
        launch: CoreLaunch,
        owner: String,
        counter: u32,
        save_dir: PathBuf,
        control: Arc<Control>,
        output: Option<&OutputPage>,
    ) -> Result<Self> {
        let prepared = snapshot::prepare(wasm).with_context(|| format!("cannot load {}", module.path))?;
        let mut config = Config::new();
        // Modules using wasm exceptions run (and are saved between reactor
        // steps), but Asyncify cannot transform them.
        config.wasm_exceptions(true);
        let engine = wasmtime::Engine::new(&config)?;
        let compiled = Module::new(&engine, &prepared.wasm).with_context(|| format!("cannot compile {}", module.path))?;
        let mut linker: Linker<RunnerState> = Linker::new(&engine);
        wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |state: &mut RunnerState| &mut state.wasi)?;
        saved_wasi::add_to_linker(&mut linker, save_point)?;

        let clock = ProgramClock::new();
        let mut builder = wasmtime_wasi::WasiCtxBuilder::new();
        match output {
            // No standard input: the page cannot be typed into.
            Some(page) => builder.stdout(page.clone()).stderr(page.clone()),
            None => builder.inherit_stdio(),
        };
        builder.args(&launch.args).monotonic_clock(clock.clone());
        for (key, value) in &launch.env {
            builder.env(key, value);
        }
        for (host, guest) in &launch.preopens {
            builder
                .preopened_dir(host, guest, wasmtime_wasi::DirPerms::all(), wasmtime_wasi::FilePerms::all())
                .with_context(|| format!("cannot preopen {host}"))?;
        }
        let point = match launch.entry {
            Entry::Reactor { .. } => Point::Pass,
            Entry::Command if prepared.layout.asyncify && prepared.layout.problems.is_empty() => Point::Save,
            Entry::Command => Point::Pause,
        };
        let state = RunnerState {
            wasi: builder.build_p1(),
            journal: FileJournal::new(launch.preopens.len()),
            clock,
            control,
            point,
            buffer: None,
            unwound_at: None,
            rewind_to: None,
        };
        let mut store = Store::new(&engine, state);
        let instance = linker
            .instantiate(&mut store, &compiled)
            .with_context(|| format!("cannot instantiate {}", module.path))?;
        Ok(Program { store, instance, layout: prepared.layout, module, launch, owner, counter, save_dir })
    }

    /// Run the program; `resuming` holds what a saved state says about its
    /// stack (nothing for a reactor).
    fn run(&mut self, resuming: Option<Option<AsyncifySaved>>) -> Result<Outcome> {
        match self.launch.entry.clone() {
            Entry::Reactor { step } => self.run_reactor(&step, resuming.is_some()),
            Entry::Command => self.run_command(resuming.flatten()),
        }
    }

    fn run_reactor(&mut self, step: &str, resuming: bool) -> Result<Outcome> {
        if !resuming {
            if let Ok(init) = self.instance.get_typed_func::<(), ()>(&mut self.store, "_initialize") {
                if let Err(e) = init.call(&mut self.store, ()) {
                    return ended(e);
                }
            }
        }
        let step = self
            .instance
            .get_typed_func::<(), i32>(&mut self.store, step)
            .with_context(|| format!("the module exports no function {step} without parameters, returning i32"))?;
        let control = self.store.data().control.clone();
        let clock = self.store.data().clock.clone();
        loop {
            match control.at(&clock, Point::Save) {
                Action::Run => {}
                Action::Quit => return Ok(Outcome::Quit),
                Action::Hibernate => match self.hibernate(None) {
                    Ok(path) => return Ok(Outcome::Hibernated(path)),
                    Err(e) => eprintln!("[wasm] cannot hibernate, going on: {e:#}"),
                },
            }
            match step.call(&mut self.store, ()) {
                Ok(status) if status < 0 => {}
                Ok(status) => return Ok(Outcome::Exited(status)),
                Err(e) => return ended(e),
            }
        }
    }

    fn run_command(&mut self, resuming: Option<AsyncifySaved>) -> Result<Outcome> {
        let start = self
            .instance
            .get_typed_func::<(), ()>(&mut self.store, "_start")
            .context("the module exports no _start (a reactor needs --step)")?;
        let mut rewind = resuming;
        loop {
            if let Some(saved) = rewind.take() {
                // Asyncify rebuilds the stack from the buffer as _start runs
                // again, and the program arrives in the call it was saved in.
                let data = self.store.data_mut();
                data.buffer = Some(saved.buffer);
                data.rewind_to = Some(saved.import);
                let start_rewind = self.instance.get_typed_func::<i32, ()>(&mut self.store, "asyncify_start_rewind")?;
                start_rewind.call(&mut self.store, saved.buffer as i32)?;
            }
            let result = start.call(&mut self.store, ());
            let Some(call) = self.store.data_mut().unwound_at.take() else {
                return match result {
                    Ok(()) => Ok(Outcome::Exited(0)),
                    Err(e) => ended(e),
                };
            };
            result.context("the program failed while unwinding its stack")?;
            let stop = self.instance.get_typed_func::<(), ()>(&mut self.store, "asyncify_stop_unwind")?;
            stop.call(&mut self.store, ())?;
            let buffer = self.store.data().buffer.expect("set before unwinding");
            let saved = AsyncifySaved { buffer, import: call.to_string() };
            match self.hibernate(Some(saved.clone())) {
                Ok(path) => return Ok(Outcome::Hibernated(path)),
                // The stack is still in memory: rebuild it and go on.
                Err(e) => {
                    eprintln!("[wasm] cannot hibernate, going on: {e:#}");
                    rewind = Some(saved);
                }
            }
        }
    }

    /// Save the program, which is at a save point, into a new file.
    fn hibernate(&mut self, asyncify: Option<AsyncifySaved>) -> Result<PathBuf> {
        let counter = self.counter + 1;
        let created = saved_state::unix_now();
        let header = Header {
            engine: saved_state::Engine::WasmSnapshot,
            mode: Mode::Hibernate,
            module: self.module.clone(),
            launch: Launch::Core(self.launch.clone()),
            m_browser_version: env!("CARGO_PKG_VERSION").to_string(),
            created_unix_secs: created,
            owner: self.owner.clone(),
            counter,
        };
        let name = saved_state::file_name(Path::new(&self.module.path), created, counter, &self.owner);
        let path = self.save_dir.join(name);
        if path.exists() {
            bail!("{} exists already", path.display());
        }
        let mut w = StateWriter::create(&path, &header)?;
        let data = self.store.data_mut();
        let wasi = saved_wasi::save(&mut data.wasi, &data.journal, &self.launch.preopens, &data.clock)
            .context("cannot save this program")?;
        w.section(saved_state::TAG_WASI, &postcard::to_allocvec(&wasi)?)?;
        if let Some(asyncify) = &asyncify {
            w.section(saved_state::TAG_ASYNCIFY, &postcard::to_allocvec(asyncify)?)?;
        }
        snapshot::save(&mut self.store, &self.instance, &self.layout, &mut w).context("cannot save this program")?;
        let path = w.finish()?;
        self.counter = counter;
        Ok(path)
    }
}

/// About how much text an `OutputPage` keeps, in bytes.
pub const OUTPUT_LIMIT: usize = 1 << 20;

/// A program's standard output and error, kept for the browser to show as
/// a text page. Only the last `OUTPUT_LIMIT` bytes or so are kept, or a
/// program that prints forever would fill the memory: wasmtime-wasi's own
/// in-memory pipe (`MemoryOutputPipe`) has a fixed capacity and traps the
/// program once it is full, so this one drops its oldest lines instead.
///
/// It is a WASI output stream twice over: as a preview 2 `OutputStream`
/// (`p2_stream`, which preview 1's `fd_write` also uses) and as a tokio
/// `AsyncWrite` (`async_stream`). Writes never wait.
#[derive(Clone, Default)]
pub struct OutputPage(Arc<Mutex<PageText>>);

#[derive(Default)]
struct PageText {
    bytes: Vec<u8>,
    /// Whether older text was dropped.
    trimmed: bool,
    /// Grows with every change, so that a viewer copies the text only then.
    version: u64,
}

impl OutputPage {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&self, bytes: &[u8]) {
        let mut page = self.0.lock().unwrap();
        page.bytes.extend_from_slice(bytes);
        // Trim in large steps, so that not every write moves the whole
        // text, and at the start of a line.
        if page.bytes.len() > OUTPUT_LIMIT + OUTPUT_LIMIT / 4 {
            let cut = page.bytes.len() - OUTPUT_LIMIT;
            let cut = page.bytes[cut..].iter().position(|&b| b == b'\n').map_or(cut, |i| cut + i + 1);
            page.bytes.drain(..cut);
            page.trimmed = true;
        }
        page.version += 1;
    }

    /// Add a line of m-browser's own, in brackets: how the program ended,
    /// say.
    pub fn note(&self, line: &str) {
        let open_line = self.0.lock().unwrap().bytes.last().is_some_and(|&b| b != b'\n');
        self.push(format!("{}[{line}]\n", if open_line { "\n" } else { "" }).as_bytes());
    }

    /// Changes whenever the text does.
    pub fn version(&self) -> u64 {
        self.0.lock().unwrap().version
    }

    /// The text (invalid UTF-8 replaced) and whether older text was dropped.
    pub fn text(&self) -> (String, bool) {
        let page = self.0.lock().unwrap();
        (String::from_utf8_lossy(&page.bytes).into_owned(), page.trimmed)
    }
}

impl wasmtime_wasi::cli::IsTerminal for OutputPage {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl wasmtime_wasi::cli::StdoutStream for OutputPage {
    fn p2_stream(&self) -> Box<dyn wasmtime_wasi_io::streams::OutputStream> {
        Box::new(self.clone())
    }

    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl wasmtime_wasi_io::streams::OutputStream for OutputPage {
    fn write(&mut self, bytes: wasmtime_wasi_io::bytes::Bytes) -> wasmtime_wasi_io::streams::StreamResult<()> {
        self.push(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> wasmtime_wasi_io::streams::StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> wasmtime_wasi_io::streams::StreamResult<usize> {
        Ok(64 * 1024)
    }
}

#[async_trait::async_trait]
impl wasmtime_wasi_io::poll::Pollable for OutputPage {
    async fn ready(&mut self) {}
}

impl tokio::io::AsyncWrite for OutputPage {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.push(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// A run that ended with an error: `exit()` and quit requests are normal
/// ends.
fn ended(e: anyhow::Error) -> Result<Outcome> {
    if let Some(exit) = e.downcast_ref::<wasmtime_wasi::I32Exit>() {
        return Ok(Outcome::Exited(exit.0));
    }
    if e.downcast_ref::<Quit>().is_some() {
        return Ok(Outcome::Quit);
    }
    Err(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::saved_state::tests::TestDir;

    /// A reactor appending a record (step number: u32, monotonic clock: u64)
    /// to log.bin at every step, for 10 steps.
    const REACTOR: &str = r#"(module
        (import "wasi_snapshot_preview1" "path_open"
          (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "clock_time_get" (func $clock_time_get (param i32 i64 i32) (result i32)))
        (memory (export "memory") 1)
        (global $fd (mut i32) (i32.const -1))
        (global $count (mut i32) (i32.const 0))
        (data (i32.const 100) "log.bin")
        (func (export "_initialize")
          (if (call $path_open (i32.const 3) (i32.const 0) (i32.const 100) (i32.const 7) (i32.const 9)
                (i64.const 0x1fffffff) (i64.const 0) (i32.const 0) (i32.const 0))
            (then unreachable))
          (global.set $fd (i32.load (i32.const 0))))
        (func (export "step") (result i32)
          (global.set $count (i32.add (global.get $count) (i32.const 1)))
          (i32.store (i32.const 204) (global.get $count))
          (if (call $clock_time_get (i32.const 1) (i64.const 1) (i32.const 208)) (then unreachable))
          (i32.store (i32.const 16) (i32.const 204))
          (i32.store (i32.const 20) (i32.const 12))
          (if (call $fd_write (global.get $fd) (i32.const 16) (i32.const 1) (i32.const 24)) (then unreachable))
          (if (result i32) (i32.lt_u (global.get $count) (i32.const 10))
            (then (i32.const -1)) (else (i32.const 0)))))"#;

    fn options(dir: &TestDir, wat: &str, step: Option<&str>) -> Options {
        let module = dir.0.join("program.wasm");
        std::fs::write(&module, wat::parse_str(wat).unwrap()).unwrap();
        let data = dir.0.join("data");
        std::fs::create_dir_all(&data).unwrap();
        Options {
            module,
            args: vec![],
            env: vec![],
            preopens: vec![(data.to_string_lossy().into_owned(), "/data".into())],
            step: step.map(Into::into),
            save_dir: dir.0.clone(),
            output: None,
            owner: None,
        }
    }

    fn hibernating_at(save_point: u64) -> Arc<Control> {
        let control = Control::new();
        control.state.lock().unwrap().hibernate_at = Some(save_point);
        control
    }

    /// The records in log.bin: (step, clock).
    fn records(dir: &TestDir) -> Vec<(u32, u64)> {
        let bytes = std::fs::read(dir.0.join("data/log.bin")).unwrap();
        assert_eq!(bytes.len() % 12, 0, "whole records only");
        bytes
            .chunks(12)
            .map(|r| (u32::from_le_bytes(r[..4].try_into().unwrap()), u64::from_le_bytes(r[4..].try_into().unwrap())))
            .collect()
    }

    fn assert_complete(records: &[(u32, u64)]) {
        let steps: Vec<u32> = records.iter().map(|r| r.0).collect();
        assert_eq!(steps, (1..=10).collect::<Vec<_>>());
        assert!(records.windows(2).all(|w| w[1].1 > w[0].1), "the clock never goes back: {records:?}");
    }

    #[test]
    fn a_reactor_hibernates_and_resumes() {
        let dir = TestDir::new("headless-reactor");
        let options = options(&dir, REACTOR, Some("step"));
        // Save point 4 is before the fourth step.
        let Outcome::Hibernated(path) = start(&options, hibernating_at(4)).unwrap() else { panic!() };
        assert_eq!(records(&dir).len(), 3);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("program-") && name.ends_with(".wasmsavestate"), "{name}");

        // Elsewhere, so that nothing depends on the working directory.
        let outcome = resume(&path, &dir.0, Control::new(), None).unwrap();
        assert_eq!(outcome, Outcome::Exited(0));
        assert_complete(&records(&dir));
    }

    #[test]
    fn an_asyncified_command_hibernates_and_resumes() {
        let dir = TestDir::new("headless-command");
        let options = options(&dir, include_str!("saved_state/testdata/writer.asyncify.wat"), None);
        // Save points: path_open, then per record clock_time_get, fd_write and
        // poll_oneoff. 14 is the fifth record's clock_time_get.
        let Outcome::Hibernated(path) = start(&options, hibernating_at(14)).unwrap() else { panic!() };
        assert_eq!(records(&dir).len(), 4);
        // A second hibernation in the same session, then a resume to the end.
        let Outcome::Hibernated(second) = resume(&path, &dir.0, hibernating_at(6), None).unwrap() else { panic!() };
        assert_eq!(records(&dir).len(), 6);
        assert!(second.to_string_lossy().contains("-2-"), "the counter goes on: {}", second.display());
        assert_eq!(resume(&second, &dir.0, Control::new(), None).unwrap(), Outcome::Exited(0));
        assert_complete(&records(&dir));
    }

    #[test]
    fn a_failed_save_lets_the_program_go_on() {
        for (name, wat, step, save_point) in [
            ("headless-failed-command", include_str!("saved_state/testdata/writer.asyncify.wat"), None, 14),
            ("headless-failed-reactor", REACTOR, Some("step"), 4),
        ] {
            let dir = TestDir::new(name);
            let mut options = options(&dir, wat, step);
            options.save_dir = dir.0.join("no such directory");
            let outcome = start(&options, hibernating_at(save_point)).unwrap();
            assert_eq!(outcome, Outcome::Exited(0), "{name}");
            assert_complete(&records(&dir));
        }
    }

    #[test]
    fn a_plain_command_runs_but_cannot_hibernate() {
        let dir = TestDir::new("headless-plain");
        let options = options(&dir, include_str!("saved_state/testdata/writer.wat"), None);
        let control = Control::new();
        control.send(Command::Hibernate);
        assert_eq!(start(&options, control).unwrap(), Outcome::Exited(0), "the request is declined");
        assert_complete(&records(&dir));
        let saved = std::fs::read_dir(&dir.0).unwrap().filter_map(|e| e.ok()).filter(|e| {
            e.path().extension().is_some_and(|x| x == saved_state::EXTENSION)
        });
        assert_eq!(saved.count(), 0);
    }

    #[test]
    fn a_resume_refuses_changed_files_and_modules() {
        let dir = TestDir::new("headless-refuse");
        let options = options(&dir, REACTOR, Some("step"));
        let Outcome::Hibernated(path) = start(&options, hibernating_at(2)).unwrap() else { panic!() };

        let log = dir.0.join("data/log.bin");
        let saved_log = std::fs::read(&log).unwrap();
        std::fs::write(&log, [saved_log.as_slice(), b"more"].concat()).unwrap();
        let e = resume(&path, &dir.0, Control::new(), None).unwrap_err();
        assert!(format!("{e:#}").contains("log.bin changed since the state was saved"), "{e:#}");

        std::fs::write(&options.module, wat::parse_str("(module)").unwrap()).unwrap();
        let e = resume(&path, &dir.0, Control::new(), None).unwrap_err();
        assert!(format!("{e:#}").contains("has changed since the state was saved"), "{e:#}");
    }

    #[test]
    fn pause_quit_and_unknown_commands() {
        let dir = TestDir::new("headless-quit");
        let options = options(&dir, REACTOR, Some("step"));
        let control = Control::new();
        control.send(Command::Pause);
        let quitter = control.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            quitter.send(Command::Quit);
        });
        assert_eq!(start(&options, control).unwrap(), Outcome::Quit);
        thread.join().unwrap();
        assert!(records(&dir).is_empty(), "paused before the first step");
        assert_eq!(Command::parse(" hibernate\n"), Some(Command::Hibernate));
        assert_eq!(Command::parse("save"), None);
    }

    #[test]
    fn output_goes_to_the_page_and_the_owner_names_saves() {
        let dir = TestDir::new("headless-page");
        // Writes "hello, page" to standard output, then counts like REACTOR.
        let hello = r#"(module
            (import "wasi_snapshot_preview1" "fd_write" (func $fd_write (param i32 i32 i32 i32) (result i32)))
            (memory (export "memory") 1)
            (data (i32.const 100) "hello, page\n")
            (global $count (mut i32) (i32.const 0))
            (func (export "step") (result i32)
              (global.set $count (i32.add (global.get $count) (i32.const 1)))
              (i32.store (i32.const 16) (i32.const 100))
              (i32.store (i32.const 20) (i32.const 12))
              (if (call $fd_write (i32.const 1) (i32.const 16) (i32.const 1) (i32.const 24)) (then unreachable))
              (if (result i32) (i32.lt_u (global.get $count) (i32.const 3))
                (then (i32.const -1)) (else (i32.const 0)))))"#;
        let page = OutputPage::new();
        let mut options = options(&dir, hello, Some("step"));
        options.output = Some(page.clone());
        options.owner = Some("0123456789ab".into());
        let Outcome::Hibernated(path) = start(&options, hibernating_at(3)).unwrap() else { panic!() };
        assert!(path.to_string_lossy().ends_with("-1-0123456789ab.wasmsavestate"), "{}", path.display());
        assert_eq!(page.text().0, "hello, page\nhello, page\n");

        let resumed = OutputPage::new();
        assert_eq!(resume(&path, &dir.0, Control::new(), Some(resumed.clone())).unwrap(), Outcome::Exited(0));
        let (text, trimmed) = resumed.text();
        assert!(text.starts_with("[resuming from ") && text.ends_with("]\nhello, page\n"), "{text:?}");
        assert!(!trimmed);
    }

    #[test]
    fn a_ticker_prints_to_its_page() {
        let dir = TestDir::new("headless-ticker");
        let page = OutputPage::new();
        let mut options = options(&dir, include_str!("saved_state/testdata/ticker.wat"), None);
        options.output = Some(page.clone());
        let control = Control::new();
        let run = {
            let control = control.clone();
            std::thread::spawn(move || start(&options, control))
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !page.text().0.contains("tick 1\n") {
            assert!(std::time::Instant::now() < deadline, "no tick: {:?}", page.text().0);
            std::thread::sleep(Duration::from_millis(10));
        }
        // It is asleep now; it quits at its next save point, the next fd_write.
        control.send(Command::Quit);
        assert_eq!(run.join().unwrap().unwrap(), Outcome::Quit);
        let text = page.text().0;
        assert!(text.starts_with("[note: this command can be paused but not saved"), "{text:?}");
        assert!(text.ends_with("]\ntick 1\n"), "{text:?}");
    }

    #[test]
    fn a_page_keeps_its_last_lines() {
        let page = OutputPage::new();
        let line = format!("{}\n", "x".repeat(99));
        let before = page.version();
        for _ in 0..(OUTPUT_LIMIT / 100) * 2 {
            page.push(line.as_bytes());
        }
        assert!(page.version() > before);
        let (text, trimmed) = page.text();
        assert!(trimmed);
        assert!(text.len() <= OUTPUT_LIMIT + OUTPUT_LIMIT / 4, "{}", text.len());
        assert!(text.len() >= OUTPUT_LIMIT - 100, "{}", text.len());
        assert!(text.starts_with('x') && text.len() % 100 == 0, "cut at the start of a line");
        page.push(b"no newline yet");
        page.note("exited with status 0");
        assert!(page.text().0.ends_with("no newline yet\n[exited with status 0]\n"));
    }

    #[test]
    fn options_are_parsed() {
        let args: Vec<String> = ["--preopen", "a=/a", "--env", "K=V=W", "--step", "go", "m.wasm", "x", "--y"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let o = parse_options(&args, PathBuf::from("/s")).unwrap();
        assert_eq!(o.preopens, [("a".to_string(), "/a".to_string())]);
        assert_eq!(o.env, [("K".to_string(), "V=W".to_string())]);
        assert_eq!((o.step.as_deref(), o.module.to_str(), o.args.as_slice()), (Some("go"), Some("m.wasm"), &["x".to_string(), "--y".to_string()][..]));
        assert!(parse_options(&["--step".to_string()], PathBuf::new()).is_err());
    }
}
