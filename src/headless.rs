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
//!   QEMU_TYPE=<script>        type text into QEMU's text console on a timer.
//!                             `<secs>:<text>` entries separated by `;`, where
//!                             `\n` in the text sends Enter. For example:
//!                             QEMU_TYPE='10:info registers\n;20:info status\n'

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

    eprintln!("[headless] running {wasm_path} with args {:?}", &argv[1..]);
    crate::wasm::run_wasm_module(wasm_path, &argv, &preopen_refs, None, queue)
}

/// Parse a QEMU_TYPE script and feed it into the event queue on a timer.
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
        steps.push((secs, text.replace("\\n", "\r")));
    }
    steps.sort_by(|a, b| a.0.total_cmp(&b.0));

    std::thread::spawn(move || {
        let mut elapsed = 0.0;
        for (at, text) in steps {
            if at > elapsed {
                std::thread::sleep(Duration::from_secs_f64(at - elapsed));
                elapsed = at;
            }
            eprintln!("[headless] typing {text:?}");
            if let Ok(mut q) = queue.lock() {
                for ch in text.chars() {
                    q.push(InputEvent {
                        event_type: EVENT_TEXT,
                        code: ch as i32,
                        value: 0,
                    });
                }
            }
        }
    });
    Ok(())
}
