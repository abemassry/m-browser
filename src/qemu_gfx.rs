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
///
/// In the browser, QEMU's frame is scaled to fit its window, which fills the
/// page area: proportions kept, centered, the rest of the window black. The
/// UI reports the window's size through the event queue
/// (`EVENT_WINDOW_SIZE`), and pointer positions, which the UI sends in
/// window pixels, are mapped back to frame pixels before QEMU sees them.
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
///   5 = text          (code=Unicode code point)
/// and two the host keeps for itself: `EVENT_REPAINT`, `EVENT_WINDOW_SIZE`.
#[derive(Clone, Copy)]
pub struct InputEvent {
    pub event_type: i32,
    pub code: i32,
    pub value: i32,
}

pub struct QemuGfxState {
    /// The size of QEMU's frames: the guest's screen as QEMU sends it.
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
    /// The last frame QEMU sent, `width` x `height`, in softbuffer's pixel
    /// format (0x00RRGGBB), to show again when the window was hidden or
    /// resized.
    pub last_frame: Vec<u32>,
    /// The window's size in physical pixels, as the UI reports it
    /// (`EVENT_WINDOW_SIZE`). 0 x 0 until then: frames are shown unscaled.
    pub window_width: u32,
    pub window_height: u32,
    /// The size softbuffer's buffer has now.
    buffer_size: (u32, u32),
}

/// A host-only event in the queue: show the last frame again. A window
/// that was hidden (an inactive tab) has lost its picture, and QEMU sends
/// an unchanged screen only about once a second. Never reaches the guest.
pub const EVENT_REPAINT: i32 = 6;

/// A host-only event in the queue: the window is now `code` x `value`
/// physical pixels. Never reaches the guest.
pub const EVENT_WINDOW_SIZE: i32 = 7;

/// Ask QEMU's display to show its last frame again, at its next refresh
/// (QEMU refreshes about 30 times a second, also while the guest is paused).
pub fn request_repaint(queue: &GfxEventQueue) {
    if let Ok(mut q) = queue.lock() {
        q.push(InputEvent { event_type: EVENT_REPAINT, code: 0, value: 0 });
    }
}

/// Tell QEMU's display the size of its window, in physical pixels: frames
/// are scaled to fit it from its next refresh on.
pub fn set_window_size(queue: &GfxEventQueue, width: u32, height: u32) {
    if let Ok(mut q) = queue.lock() {
        let size = |n: u32| n.min(i32::MAX as u32) as i32;
        q.push(InputEvent { event_type: EVENT_WINDOW_SIZE, code: size(width), value: size(height) });
    }
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
            last_frame: Vec::new(),
            window_width: 0,
            window_height: 0,
            buffer_size: (0, 0),
        }
    }

    /// Where the frame is drawn in the window: (x, y, width, height) in
    /// window pixels. The frame is scaled to fit, keeping its proportions,
    /// and centered. `None` while the window's or the frame's size is not
    /// known.
    fn view(&self) -> Option<(u32, u32, u32, u32)> {
        let (fw, fh) = (self.width as u64, self.height as u64);
        let (ww, wh) = (self.window_width as u64, self.window_height as u64);
        if fw == 0 || fh == 0 || ww == 0 || wh == 0 {
            return None;
        }
        // Compare ww / fw with wh / fh without dividing.
        let (vw, vh) = if ww * fh <= wh * fw {
            (ww, (fh * ww / fw).max(1))
        } else {
            ((fw * wh / fh).max(1), wh)
        };
        Some((((ww - vw) / 2) as u32, ((wh - vh) / 2) as u32, vw as u32, vh as u32))
    }

    /// A pointer position from the UI, in window pixels, as a position in
    /// the frame, which is what QEMU expects. Other events are unchanged.
    fn to_frame(&self, event: InputEvent) -> InputEvent {
        if event.event_type != 1 {
            return event;
        }
        let Some((vx, vy, vw, vh)) = self.view() else { return event };
        let (start, shown, size) = if event.code == 0 { (vx, vw, self.width) } else { (vy, vh, self.height) };
        let at = (event.value as i64 - start as i64).clamp(0, shown as i64 - 1);
        InputEvent { value: (at * size as i64 / shown as i64) as i32, ..event }
    }

    /// Take what the UI queued: input for the guest goes to
    /// `pending_events`, and host-only events are handled here. Returns
    /// whether the window must be painted again.
    fn take_events(&mut self, queue: &GfxEventQueue) -> bool {
        let Ok(mut queue) = queue.lock() else { return false };
        let mut redraw = false;
        for event in queue.drain(..) {
            match event.event_type {
                EVENT_REPAINT => redraw = true,
                EVENT_WINDOW_SIZE => {
                    let size = (event.code.max(0) as u32, event.value.max(0) as u32);
                    if size != (self.window_width, self.window_height) {
                        (self.window_width, self.window_height) = size;
                        redraw = true;
                    }
                }
                _ => {
                    let event = self.to_frame(event);
                    self.pending_events.push(event);
                }
            }
        }
        redraw
    }

    /// Paint the whole window: the last frame scaled to fit (see `view`),
    /// black around it. Every pixel is painted, so nothing of what was on
    /// the screen before the window appeared stays visible. Without a known
    /// window size, the frame is shown as it is.
    fn present(&mut self) {
        let view = self.view();
        let (fw, fh) = (self.width as usize, self.height as usize);
        let (bw, bh) = if self.window_width > 0 && self.window_height > 0 {
            (self.window_width, self.window_height)
        } else {
            (self.width, self.height)
        };
        let (Some(nw), Some(nh)) = (NonZeroU32::new(bw), NonZeroU32::new(bh)) else { return };
        let Some(surface) = self.sb_surface.as_mut() else { return };
        if self.buffer_size != (bw, bh) {
            if let Err(e) = surface.resize(nw, nh) {
                eprintln!("[qemu-gfx] softbuffer resize failed: {e}");
                return;
            }
            self.buffer_size = (bw, bh);
        }
        let frame = &self.last_frame;
        let has_frame = fw > 0 && fh > 0 && frame.len() == fw * fh;
        let mut buffer = match surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(e) => {
                eprintln!("[qemu-gfx] softbuffer buffer_mut failed: {e}");
                return;
            }
        };
        match view {
            Some(view) if has_frame => draw_scaled(&mut buffer, bw as usize, frame, fw, fh, view),
            None if has_frame => {
                let n = buffer.len().min(frame.len());
                buffer[..n].copy_from_slice(&frame[..n]);
            }
            _ => buffer.fill(0),
        }
        if let Err(e) = buffer.present() {
            eprintln!("[qemu-gfx] softbuffer present failed: {e}");
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

/// Fill `buffer`, a window `width` pixels wide, with `frame` (`fw` x `fh`)
/// scaled into `view` (x, y, width, height), and black around it. Nearest
/// neighbour: each window pixel takes the frame pixel it falls on.
fn draw_scaled(buffer: &mut [u32], width: usize, frame: &[u32], fw: usize, fh: usize, view: (u32, u32, u32, u32)) {
    let (vx, vy, vw, vh) = (view.0 as usize, view.1 as usize, view.2 as usize, view.3 as usize);
    let columns: Vec<usize> = (0..vw).map(|x| x * fw / vw).collect();
    for (y, row) in buffer.chunks_exact_mut(width).enumerate() {
        if y < vy || y >= vy + vh {
            row.fill(0);
            continue;
        }
        let sy = (y - vy) * fh / vh;
        let source = &frame[sy * fw..(sy + 1) * fw];
        row[..vx].fill(0);
        for (pixel, &sx) in row[vx..vx + vw].iter_mut().zip(&columns) {
            *pixel = source[sx];
        }
        row[vx + vw..].fill(0);
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
    let eq = event_queue.clone();
    linker.func_wrap(
        "env",
        "host_gfx_init",
        move |caller: Caller<'_, QemuHostState>, width: i32, height: i32| {
            eprintln!("[qemu-gfx] host_gfx_init({width}, {height})");

            let gfx = caller.data().gfx.clone();
            let mut gfx = gfx.lock().unwrap();
            gfx.width = width as u32;
            gfx.height = height as u32;

            // Create softbuffer surface from the wasi_surface if available
            if let Some(wasi_surface) = gfx.wasi_surface.clone() {
                match softbuffer::Context::new(Arc::clone(&wasi_surface)) {
                    Ok(context) => match softbuffer::Surface::new(&context, wasi_surface) {
                        Ok(sb_surface) => {
                            gfx.sb_context = Some(context);
                            gfx.sb_surface = Some(sb_surface);
                            gfx.buffer_size = (0, 0);
                            gfx.initialized = true;
                            eprintln!("[qemu-gfx] softbuffer surface created successfully");
                            // Paint the window at once (black until the
                            // first frame), in the size the UI reported.
                            gfx.take_events(&eq);
                            gfx.present();
                        }
                        Err(e) => {
                            eprintln!("[qemu-gfx] failed to create softbuffer surface: {e}");
                        }
                    },
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
    let eq = event_queue.clone();
    linker.func_wrap(
        "env",
        "host_gfx_update_framebuffer",
        move |caller: Caller<'_, QemuHostState>,
              data_ptr: i32,
              data_len: i32,
              width: i32,
              height: i32,
              stride: i32| {
            let _timer = crate::qemu_profile::time_host(crate::qemu_profile::HostWork::Display);
            let ptr = data_ptr as u32 as usize;
            let len = data_len as u32 as usize;
            let w = width as u32;
            let h = height as u32;
            let s = stride as usize;

            // Convert BGRA8 guest pixels to softbuffer's native u32 format (0x00RRGGBB)
            // softbuffer uses native-endian u32 where each pixel is 0x00RRGGBB
            let converted = crate::qemu_jit::with_guest(&caller, ptr, len, |guest_pixels| {
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
                buffer_data
            });
            let Some(buffer_data) = converted else {
                eprintln!("[qemu-gfx] update_framebuffer: out of bounds ({ptr}+{len})");
                return;
            };

            let gfx = caller.data().gfx.clone();
            let mut gfx = gfx.lock().unwrap();
            let gfx = &mut *gfx;

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

            // Learn a new window size before drawing.
            gfx.take_events(&eq);
            gfx.width = w;
            gfx.height = h;
            gfx.last_frame = buffer_data;
            gfx.present();
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
        |caller: Caller<'_, QemuHostState>, width: i32, height: i32| {
            eprintln!("[qemu-gfx] host_gfx_resize({width}, {height})");
            // The next frame has this size; `present` sizes the buffer.
            let gfx = caller.data().gfx.clone();
            let mut gfx = gfx.lock().unwrap();
            gfx.width = width as u32;
            gfx.height = height as u32;
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
            // Drain events from the shared queue into host state, and
            // repaint if the UI asked for it or resized the window.
            let gfx = caller.data().gfx.clone();
            let mut gfx = gfx.lock().unwrap();
            if gfx.take_events(&eq) {
                gfx.present();
            }

            let max = max_events as usize;
            let count = gfx.pending_events.len().min(max);

            if count == 0 {
                return 0;
            }

            // Collect events to write
            let events: Vec<InputEvent> = gfx.pending_events.drain(..count).collect();
            drop(gfx);

            // Write events to guest memory: 3 x i32 per event
            let written = crate::qemu_jit::with_guest_mut(
                &mut caller,
                event_buf_ptr as u32 as usize,
                count * 12,
                |buf| {
                    for (i, ev) in events.iter().enumerate() {
                        let base = i * 12;
                        buf[base..base + 4].copy_from_slice(&ev.event_type.to_le_bytes());
                        buf[base + 4..base + 8].copy_from_slice(&ev.code.to_le_bytes());
                        buf[base + 8..base + 12].copy_from_slice(&ev.value.to_le_bytes());
                    }
                },
            );
            if written.is_none() {
                eprintln!("[qemu-gfx] poll_events: buffer out of bounds");
                return 0;
            }

            count as i32
        },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(frame: (u32, u32), window: (u32, u32)) -> QemuGfxState {
        let mut gfx = QemuGfxState::new();
        (gfx.width, gfx.height) = frame;
        (gfx.window_width, gfx.window_height) = window;
        gfx
    }

    #[test]
    fn the_frame_fits_the_window_keeping_its_proportions() {
        // A 1280x800 desktop in a wider window: as tall as the window,
        // centered between black bars.
        assert_eq!(state((1280, 800), (2400, 1200)).view(), Some((240, 0, 1920, 1200)));
        // In a taller window: as wide as the window.
        assert_eq!(state((1440, 800), (1440, 1000)).view(), Some((0, 100, 1440, 800)));
        // Smaller windows scale it down.
        assert_eq!(state((1280, 800), (640, 600)).view(), Some((0, 100, 640, 400)));
        assert_eq!(state((1280, 800), (0, 0)).view(), None);
        assert_eq!(state((0, 0), (640, 480)).view(), None);
    }

    #[test]
    fn pointer_positions_are_mapped_into_the_frame() {
        let gfx = state((1280, 800), (2400, 1200));
        let at = |code: i32, value: i32| gfx.to_frame(InputEvent { event_type: 1, code, value }).value;
        assert_eq!(at(0, 240), 0, "the frame's left edge");
        assert_eq!(at(0, 2159), 1279, "its right edge");
        assert_eq!(at(0, 1200), 640, "the middle");
        assert_eq!(at(0, 10), 0, "the black bar beside it: clamped");
        assert_eq!(at(1, 600), 400);
        let key = gfx.to_frame(InputEvent { event_type: 3, code: 30, value: 1 });
        assert_eq!((key.code, key.value), (30, 1), "keys are not changed");
        let unknown = state((1280, 800), (0, 0));
        assert_eq!(unknown.to_frame(InputEvent { event_type: 1, code: 0, value: 500 }).value, 500);
    }

    #[test]
    fn the_whole_window_is_painted() {
        // A 2x2 frame in an 8x4 window: doubled, centered, black bars.
        let gfx = state((2, 2), (8, 4));
        let view = gfx.view().unwrap();
        assert_eq!(view, (2, 0, 4, 4));
        let mut window = vec![9u32; 8 * 4]; // what was on the screen before
        draw_scaled(&mut window, 8, &[1, 2, 3, 4], 2, 2, view);
        assert_eq!(
            window,
            [
                0, 0, 1, 1, 2, 2, 0, 0, //
                0, 0, 1, 1, 2, 2, 0, 0, //
                0, 0, 3, 3, 4, 4, 0, 0, //
                0, 0, 3, 3, 4, 4, 0, 0,
            ]
        );
        // Shrunk into a 3x1 window: one pixel, the frame's top left.
        let mut small = vec![9u32; 3];
        draw_scaled(&mut small, 3, &[1, 2, 3, 4], 2, 2, state((2, 2), (3, 1)).view().unwrap());
        assert_eq!(small, [0, 1, 0]);
    }

    #[test]
    fn host_events_stay_in_the_host() {
        let queue = new_event_queue();
        set_window_size(&queue, 2400, 1200);
        request_repaint(&queue);
        queue.lock().unwrap().push(InputEvent { event_type: 1, code: 0, value: 1200 });
        let mut gfx = state((1280, 800), (0, 0));
        assert!(gfx.take_events(&queue), "a new size or a repaint means painting again");
        assert_eq!((gfx.window_width, gfx.window_height), (2400, 1200));
        assert_eq!(gfx.pending_events.len(), 1, "only the guest's event is left");
        assert_eq!(gfx.pending_events[0].value, 640, "mapped with the new size");
        set_window_size(&queue, 2400, 1200);
        assert!(!gfx.take_events(&queue), "the same size again: nothing to do");
    }
}
