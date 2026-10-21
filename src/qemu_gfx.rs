/// QEMU GFX Host Extension for m-browser
///
/// Implements the 4 host display functions that the QEMU wasm binary imports
/// from the `env` module:
///   - `host_gfx_init(width, height)`
///   - `host_gfx_update_framebuffer(data_ptr, data_len, width, height, stride)`
///   - `host_gfx_resize(width, height)`
///   - `host_gfx_poll_events(event_buf, max_events) -> count`
///
/// Uses `softbuffer` for CPU-based framebuffer blitting to the window surface.
/// This avoids the complexity of setting up a full wgpu render pipeline just
/// to display QEMU's BGRA8 framebuffer data.
use anyhow::Result;
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};
use wasmtime::{Caller, Linker};

use crate::qemu_jit::QemuHostState;

// ---------------------------------------------------------------------------
//  GFX State — held inside QemuHostState
// ---------------------------------------------------------------------------

/// Input event queued by the host for delivery to the guest.
/// Each event is [type, code, value] matching the C-side protocol:
///   1 = pointer_move  (code=0:x 1:y, value=position)
///   2 = pointer_btn   (code=button_id, value=0:up 1:down)
///   3 = key           (code=keycode, value=0:up 1:down)
///   4 = resize        (code=width, value=height)
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub event_type: i32,
    pub code: i32,
    pub value: i32,
}

pub struct QemuGfxState {
    /// Display dimensions
    pub width: u32,
    pub height: u32,
    /// The softbuffer context (needed for surface lifetime)
    pub sb_context: Option<softbuffer::Context<Arc<wasi_surface_wasmtime::Surface>>>,
    /// The softbuffer surface for blitting pixels to the window
    pub sb_surface: Option<
        softbuffer::Surface<
            Arc<wasi_surface_wasmtime::Surface>,
            Arc<wasi_surface_wasmtime::Surface>,
        >,
    >,
    /// Reference to the wasi surface (needed for softbuffer lifetime)
    pub wasi_surface: Option<Arc<wasi_surface_wasmtime::Surface>>,
    /// Input events pending delivery to guest
    pub pending_events: Vec<InputEvent>,
    /// Whether init has been called
    pub initialized: bool,
    /// Headless only: when set, frames are written to this PNG (throttled)
    /// instead of a window. Configured from the QEMU_SNAPSHOT env var.
    pub snapshot_path: Option<std::path::PathBuf>,
    /// When the last snapshot was written, for throttling.
    pub last_snapshot: Option<std::time::Instant>,
}

impl QemuGfxState {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            sb_context: None,
            sb_surface: None,
            wasi_surface: None,
            pending_events: Vec::new(),
            initialized: false,
            snapshot_path: std::env::var_os("QEMU_SNAPSHOT").map(Into::into),
            last_snapshot: None,
        }
    }

    /// Queue an input event for delivery to the guest on next poll
    pub fn queue_event(&mut self, event_type: i32, code: i32, value: i32) {
        self.pending_events.push(InputEvent {
            event_type,
            code,
            value,
        });
    }
}

// ---------------------------------------------------------------------------
//  Shared reference for event injection from the main thread
// ---------------------------------------------------------------------------

/// A thread-safe handle to the GFX event queue, so the main winit event loop
/// can inject pointer/keyboard events.
pub type GfxEventQueue = Arc<Mutex<Vec<InputEvent>>>;

/// Create a new shared event queue.
pub fn new_event_queue() -> GfxEventQueue {
    Arc::new(Mutex::new(Vec::new()))
}

// ---------------------------------------------------------------------------
//  Add GFX host functions to a core-module Linker<QemuHostState>
// ---------------------------------------------------------------------------

pub fn add_gfx_to_linker(
    linker: &mut Linker<QemuHostState>,
    event_queue: GfxEventQueue,
) -> Result<()> {
    // -----------------------------------------------------------------------
    //  env::host_gfx_init
    //
    //  void host_gfx_init(int32_t width, int32_t height)
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "host_gfx_init",
        |mut caller: Caller<'_, QemuHostState>, width: i32, height: i32| {
            eprintln!("[qemu-gfx] host_gfx_init({width}, {height})");

            let gfx = &mut caller.data_mut().gfx;
            gfx.width = width as u32;
            gfx.height = height as u32;

            // Create softbuffer surface from the wasi_surface if available
            if let Some(ref wasi_surface) = gfx.wasi_surface {
                let ws_display = Arc::clone(wasi_surface);
                let ws_window = Arc::clone(wasi_surface);
                match softbuffer::Context::new(ws_display) {
                    Ok(context) => {
                        match softbuffer::Surface::new(&context, ws_window) {
                            Ok(mut sb_surface) => {
                                // Set initial buffer size
                                let w = NonZeroU32::new(width as u32)
                                    .unwrap_or(NonZeroU32::new(1).unwrap());
                                let h = NonZeroU32::new(height as u32)
                                    .unwrap_or(NonZeroU32::new(1).unwrap());
                                if let Err(e) = sb_surface.resize(w, h) {
                                    eprintln!("[qemu-gfx] softbuffer resize failed: {e}");
                                }
                                gfx.sb_context = Some(context);
                                gfx.sb_surface = Some(sb_surface);
                                gfx.initialized = true;
                                eprintln!("[qemu-gfx] softbuffer surface created successfully");
                            }
                            Err(e) => {
                                eprintln!("[qemu-gfx] failed to create softbuffer surface: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[qemu-gfx] failed to create softbuffer context: {e}");
                    }
                }
            } else {
                eprintln!("[qemu-gfx] no wasi surface available (running headless?)");
                gfx.initialized = true; // still mark as initialized for -nographic fallback
            }
        },
    )?;

    // -----------------------------------------------------------------------
    //  env::host_gfx_update_framebuffer
    //
    //  void host_gfx_update_framebuffer(
    //      const uint8_t *data_ptr,  // i32 guest memory offset
    //      int32_t data_len,
    //      int32_t width,
    //      int32_t height,
    //      int32_t stride
    //  )
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "host_gfx_update_framebuffer",
        |mut caller: Caller<'_, QemuHostState>,
         data_ptr: i32,
         data_len: i32,
         width: i32,
         height: i32,
         stride: i32| {
            let _timer = crate::qemu_profile::time_host(crate::qemu_profile::HostWork::Display);
            // Get guest memory
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(m)) => m,
                _ => {
                    eprintln!("[qemu-gfx] update_framebuffer: guest memory not found");
                    return;
                }
            };

            let mem_data = memory.data(&caller);
            let ptr = data_ptr as usize;
            let len = data_len as usize;
            let w = width as u32;
            let h = height as u32;
            let s = stride as usize;

            if ptr + len > mem_data.len() {
                eprintln!(
                    "[qemu-gfx] update_framebuffer: OOB ({ptr}..{}, mem {})",
                    ptr + len,
                    mem_data.len()
                );
                return;
            }

            let guest_pixels = &mem_data[ptr..ptr + len];

            // Convert BGRA8 guest pixels to softbuffer's native u32 format (0x00RRGGBB)
            // softbuffer uses native-endian u32 where each pixel is 0x00RRGGBB
            let pixel_count = (w * h) as usize;
            let mut buffer_data: Vec<u32> = Vec::with_capacity(pixel_count);

            for row in 0..h as usize {
                let row_start = row * s;
                for col in 0..w as usize {
                    let offset = row_start + col * 4;
                    if offset + 3 < guest_pixels.len() {
                        let b = guest_pixels[offset] as u32;
                        let g = guest_pixels[offset + 1] as u32;
                        let r = guest_pixels[offset + 2] as u32;
                        // Skip alpha (offset + 3)
                        buffer_data.push((r << 16) | (g << 8) | b);
                    } else {
                        buffer_data.push(0);
                    }
                }
            }

            // Blit to softbuffer
            let gfx = &mut caller.data_mut().gfx;

            // Headless: no window to draw into, so optionally save the frame
            // as a PNG for inspection. Throttled to about once per second;
            // written to a temp file and renamed so readers never see a
            // half-written image.
            if gfx.sb_surface.is_none() {
                if let Some(path) = gfx.snapshot_path.clone() {
                    let due = gfx
                        .last_snapshot
                        .map_or(true, |t| t.elapsed() >= std::time::Duration::from_secs(1));
                    if due {
                        gfx.last_snapshot = Some(std::time::Instant::now());
                        let mut rgb = Vec::with_capacity(buffer_data.len() * 3);
                        for px in &buffer_data {
                            rgb.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8]);
                        }
                        let tmp = path.with_extension("png.tmp");
                        let res = image::save_buffer_with_format(
                            &tmp,
                            &rgb,
                            w,
                            h,
                            image::ColorType::Rgb8,
                            image::ImageFormat::Png,
                        )
                        .map_err(anyhow::Error::from)
                        .and_then(|_| std::fs::rename(&tmp, &path).map_err(anyhow::Error::from));
                        if let Err(e) = res {
                            eprintln!("[qemu-gfx] snapshot to {} failed: {e}", path.display());
                        }
                    }
                }
                return;
            }

            if let Some(ref mut sb_surface) = gfx.sb_surface {
                // Ensure buffer size matches
                let nw = NonZeroU32::new(w).unwrap_or(NonZeroU32::new(1).unwrap());
                let nh = NonZeroU32::new(h).unwrap_or(NonZeroU32::new(1).unwrap());
                if gfx.width != w || gfx.height != h {
                    if let Err(e) = sb_surface.resize(nw, nh) {
                        eprintln!("[qemu-gfx] softbuffer resize failed: {e}");
                        return;
                    }
                    gfx.width = w;
                    gfx.height = h;
                }

                match sb_surface.buffer_mut() {
                    Ok(mut buffer) => {
                        let copy_len = buffer.len().min(buffer_data.len());
                        buffer[..copy_len].copy_from_slice(&buffer_data[..copy_len]);
                        if let Err(e) = buffer.present() {
                            eprintln!("[qemu-gfx] softbuffer present failed: {e}");
                        }
                    }
                    Err(e) => {
                        eprintln!("[qemu-gfx] softbuffer buffer_mut failed: {e}");
                    }
                }
            }
        },
    )?;

    // -----------------------------------------------------------------------
    //  env::host_gfx_resize
    //
    //  void host_gfx_resize(int32_t width, int32_t height)
    // -----------------------------------------------------------------------
    linker.func_wrap(
        "env",
        "host_gfx_resize",
        |mut caller: Caller<'_, QemuHostState>, width: i32, height: i32| {
            eprintln!("[qemu-gfx] host_gfx_resize({width}, {height})");
            let gfx = &mut caller.data_mut().gfx;
            gfx.width = width as u32;
            gfx.height = height as u32;

            if let Some(ref mut sb_surface) = gfx.sb_surface {
                let w = NonZeroU32::new(width as u32).unwrap_or(NonZeroU32::new(1).unwrap());
                let h = NonZeroU32::new(height as u32).unwrap_or(NonZeroU32::new(1).unwrap());
                if let Err(e) = sb_surface.resize(w, h) {
                    eprintln!("[qemu-gfx] softbuffer resize failed: {e}");
                }
            }
        },
    )?;

    // -----------------------------------------------------------------------
    //  env::host_gfx_poll_events
    //
    //  int32_t host_gfx_poll_events(int32_t *event_buf, int32_t max_events)
    //
    //  Each event is 3 x i32 = 12 bytes: [type, code, value]
    //  Returns number of events written.
    // -----------------------------------------------------------------------
    let eq = event_queue.clone();
    linker.func_wrap(
        "env",
        "host_gfx_poll_events",
        move |mut caller: Caller<'_, QemuHostState>, event_buf_ptr: i32, max_events: i32| -> i32 {
            // Drain events from the shared queue into host state
            if let Ok(mut queue) = eq.lock() {
                let gfx = &mut caller.data_mut().gfx;
                for ev in queue.drain(..) {
                    gfx.pending_events.push(ev);
                }
            }

            let max = max_events as usize;
            let gfx = &mut caller.data_mut().gfx;
            let count = gfx.pending_events.len().min(max);

            if count == 0 {
                return 0;
            }

            // Collect events to write
            let events: Vec<InputEvent> = gfx.pending_events.drain(..count).collect();

            // Write events to guest memory
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(m)) => m,
                _ => return 0,
            };

            let mem_data = memory.data_mut(&mut caller);
            let buf_offset = event_buf_ptr as usize;
            let needed = count * 12; // 3 x i32 per event

            if buf_offset + needed > mem_data.len() {
                eprintln!("[qemu-gfx] poll_events: buffer OOB");
                return 0;
            }

            for (i, ev) in events.iter().enumerate() {
                let base = buf_offset + i * 12;
                mem_data[base..base + 4].copy_from_slice(&ev.event_type.to_le_bytes());
                mem_data[base + 4..base + 8].copy_from_slice(&ev.code.to_le_bytes());
                mem_data[base + 8..base + 12].copy_from_slice(&ev.value.to_le_bytes());
            }

            count as i32
        },
    )?;

    Ok(())
}
