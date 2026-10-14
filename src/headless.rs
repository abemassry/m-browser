//! Headless QEMU runner for development and automated testing.
//!
//! Runs a QEMU wasm module through exactly the same wasmtime + JIT + WASI host
//! as the GUI (`wasm::run_wasm_module`), but without creating a window, so the
//! emulator can be exercised from a terminal or script.
//!
//! Usage:
//!   m-browser --qemu-headless [--preopen HOST=GUEST]... <qemu.wasm> [qemu args...]
//!
//! As in the GUI, `./pc-bios` is preopened as `/bios` and `.` as `/data`.
//! Extra directories (e.g. somewhere to write logs) can be added with
//! `--preopen`.
//!
//! Environment:
//!   QEMU_SNAPSHOT=<file.png>  write the display to this PNG about once a second
//!   QEMU_TYPE=<script>        type text on a timer, both into QEMU's text
//!                             consoles and as key presses for the guest.
//!                             `<secs>:<text>` entries separated by `;`, where
//!                             `\n` in the text sends Enter. For example:
//!                             QEMU_TYPE='10:info registers\n;20:info status\n'
//!   QEMU_TYPE_FIFO=<path>     type each line written to this named pipe
//!                             (see mkfifo), followed by Enter; lines starting
//!                             with `!` are pointer commands (`!move X Y`,
//!                             `!click BUTTON`; see `pointer_command`)
//!   QEMU_QUIT_AFTER=<secs>    ask QEMU to quit after this long, as the GUI
//!                             does when its window closes

use anyhow::{bail, Context, Result};
use std::time::Duration;

use crate::qemu_gfx::{GfxEventQueue, InputEvent};

/// Event type the guest display backend treats as a typed character.
const EVENT_TEXT: i32 = 5;

pub fn run(args: &[String]) -> Result<()> {
    let mut preopens: Vec<(String, String)> = vec![
        ("pc-bios".into(), "/bios".into()),
        (".".into(), "/data".into()),
    ];

    let mut rest = args;
    while let Some(flag) = rest.first() {
        if flag != "--preopen" {
            break;
        }
        let spec = rest.get(1).context("--preopen needs HOST=GUEST")?;
        let (host, guest) = spec
            .split_once('=')
            .with_context(|| format!("--preopen expects HOST=GUEST, got {spec:?}"))?;
        preopens.push((host.into(), guest.into()));
        rest = &rest[2..];
    }

    let Some((wasm_path, qemu_args)) = rest.split_first() else {
        bail!("usage: m-browser --qemu-headless [--preopen HOST=GUEST]... <qemu.wasm> [qemu args...]");
    };

    // argv[0] for the guest, then the caller's arguments verbatim.
    let argv: Vec<&str> = std::iter::once("qemu-system-x86_64")
        .chain(qemu_args.iter().map(String::as_str))
        .collect();
    let preopen_refs: Vec<(&str, &str)> = preopens
        .iter()
        .map(|(h, g)| (h.as_str(), g.as_str()))
        .collect();

    let queue = crate::qemu_gfx::new_event_queue();
    if let Ok(script) = std::env::var("QEMU_TYPE") {
        spawn_typist(&script, queue.clone())?;
    }
    if let Some(path) = std::env::var_os("QEMU_TYPE_FIFO") {
        spawn_fifo_typist(path.into(), queue.clone());
    }

    let stop = match std::env::var("QEMU_QUIT_AFTER") {
        Ok(secs) => {
            let secs: f64 = secs.parse().context("QEMU_QUIT_AFTER must be a number of seconds")?;
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs_f64(secs));
                eprintln!("[headless] QEMU_QUIT_AFTER: asking QEMU to quit");
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            });
            Some(stop)
        }
        Err(_) => None,
    };

    eprintln!("[headless] running {wasm_path} with args {:?}", &argv[1..]);
    crate::wasm::run_wasm_module(wasm_path, &argv, &preopen_refs, None, queue, stop)
}

/// Linux evdev code for a character on a US keyboard, and whether it needs
/// Shift. Used so typed text also reaches the guest's own keyboard.
fn char_to_evdev(ch: char) -> Option<(i32, bool)> {
    const LETTERS: &[u8; 26] = &[
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const DIGITS: &[u8; 10] = &[11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    let key = match ch {
        'a'..='z' => (LETTERS[(ch as u8 - b'a') as usize] as i32, false),
        'A'..='Z' => (LETTERS[(ch as u8 - b'A') as usize] as i32, true),
        '0'..='9' => (DIGITS[(ch as u8 - b'0') as usize] as i32, false),
        '\r' | '\n' => (28, false),
        '\t' => (15, false),
        '\u{1b}' => (1, false),
        '\u{7f}' | '\u{8}' => (14, false),
        ' ' => (57, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        _ => {
            let i = SHIFTED_DIGITS.find(ch)?;
            (DIGITS[i] as i32, true)
        }
    };
    Some(key)
}

/// Parse a QEMU_TYPE script and feed it into the event queue on a timer.
///
/// Each character is sent both as a key press/release (for the guest's
/// keyboard, i.e. graphic consoles) and as text (for QEMU's text consoles,
/// such as the monitor); each kind of console ignores the other. `\n` is
/// Enter, `\t` Tab and `\e` Escape.
fn spawn_typist(script: &str, queue: GfxEventQueue) -> Result<()> {
    let mut steps: Vec<(f64, String)> = Vec::new();
    for entry in script.split(';').filter(|e| !e.is_empty()) {
        let (secs, text) = entry
            .split_once(':')
            .with_context(|| format!("QEMU_TYPE entry {entry:?} is not <secs>:<text>"))?;
        let secs: f64 = secs
            .trim()
            .parse()
            .with_context(|| format!("bad delay in QEMU_TYPE entry {entry:?}"))?;
        let text = text.replace("\\n", "\r").replace("\\t", "\t").replace("\\e", "\u{1b}");
        steps.push((secs, text));
    }
    steps.sort_by(|a, b| a.0.total_cmp(&b.0));

    std::thread::spawn(move || {
        let mut elapsed = 0.0;
        for (at, text) in steps {
            if at > elapsed {
                std::thread::sleep(Duration::from_secs_f64(at - elapsed));
                elapsed = at;
            }
            type_text(&queue, &text);
        }
    });
    Ok(())
}

/// Type each line written to the named pipe at `path` (create it with
/// mkfifo), followed by Enter. Lets a person or script drive a running
/// headless guest, e.g. `echo root > /tmp/type.fifo`.
fn spawn_fifo_typist(path: std::path::PathBuf, queue: GfxEventQueue) {
    use std::io::BufRead;

    std::thread::spawn(move || loop {
        // Opening blocks until a writer appears; reopen after each writer.
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[headless] cannot open {}: {e}", path.display());
                return;
            }
        };
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            if let Some(cmd) = line.strip_prefix('!') {
                pointer_command(&queue, cmd);
            } else {
                type_text(&queue, &format!("{line}\r"));
            }
        }
    });
}

/// Pointer commands for the FIFO typist, in window pixels:
/// `!move X Y`, `!click BUTTON` (QEMU button codes: 0 left, 1 middle,
/// 2 right), `!down BUTTON` and `!up BUTTON`.
fn pointer_command(queue: &GfxEventQueue, cmd: &str) {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let num = |i: usize| words.get(i).and_then(|w| w.parse::<i32>().ok());
    let mut events: Vec<(i32, i32, i32)> = Vec::new();
    match (words.first().copied(), num(1), num(2)) {
        (Some("move"), Some(x), Some(y)) => {
            events.push((1, 0, x));
            events.push((1, 1, y));
        }
        (Some("click"), Some(b), _) => {
            events.push((2, b, 1));
            events.push((2, b, 0));
        }
        (Some("down"), Some(b), _) => events.push((2, b, 1)),
        (Some("up"), Some(b), _) => events.push((2, b, 0)),
        _ => {
            eprintln!("[headless] unknown pointer command {cmd:?}");
            return;
        }
    }
    eprintln!("[headless] pointer {cmd}");
    if let Ok(mut q) = queue.lock() {
        for (event_type, code, value) in events {
            q.push(InputEvent { event_type, code, value });
        }
    }
}

/// Send `text` as key presses for the guest plus text for QEMU's consoles.
fn type_text(queue: &GfxEventQueue, text: &str) {
    const EVENT_KEY: i32 = 3;
    const KEY_LEFTSHIFT: i32 = 42;

    eprintln!("[headless] typing {text:?}");
    for ch in text.chars() {
        let Ok(mut q) = queue.lock() else { return };
        let key = |q: &mut Vec<InputEvent>, code: i32, down: bool| {
            q.push(InputEvent {
                event_type: EVENT_KEY,
                code,
                value: down as i32,
            })
        };
        if let Some((code, shift)) = char_to_evdev(ch) {
            if shift {
                key(&mut q, KEY_LEFTSHIFT, true);
            }
            key(&mut q, code, true);
            key(&mut q, code, false);
            if shift {
                key(&mut q, KEY_LEFTSHIFT, false);
            }
        }
        q.push(InputEvent {
            event_type: EVENT_TEXT,
            code: ch as i32,
            value: 0,
        });
        drop(q);
        // Roughly human typing speed, so the guest sees separate keys.
        std::thread::sleep(Duration::from_millis(60));
    }
}
