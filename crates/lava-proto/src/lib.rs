//! Flat, application-agnostic protocol between Lava's native UI thread and single Nessa VM thread.
//!
//! By design (Principles 2 & 8), this crate has zero knowledge of buffers, modes, keymaps,
//! or editors. The Nessa VM commits immutable [`RenderFrame`] snapshots into [`FrameSlot`],
//! and the native GTK4 UI thread pushes [`UiEvent`] values into [`EventQueue`].

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Bitflags for keyboard and pointer modifier state.
pub mod mods {
    pub const CTRL: u32 = 1 << 0;
    pub const ALT: u32 = 1 << 1;
    pub const SHIFT: u32 = 1 << 2;
    pub const SUPER: u32 = 1 << 3;
}

/// RGBA color packed as `0xRRGGBBAA`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColorRgba(pub u32);

impl ColorRgba {
    pub const fn from_rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self(((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | (a as u32))
    }

    pub const fn from_u32(packed: u32) -> Self {
        Self(packed)
    }

    pub fn components_f64(self) -> (f64, f64, f64, f64) {
        let r = ((self.0 >> 24) & 0xff) as f64 / 255.0;
        let g = ((self.0 >> 16) & 0xff) as f64 / 255.0;
        let b = ((self.0 >> 8) & 0xff) as f64 / 255.0;
        let a = (self.0 & 0xff) as f64 / 255.0;
        (r, g, b, a)
    }
}

/// Visual caret geometry style for text/canvas cursors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaretShape {
    Block = 0,
    Beam = 1,
    Underline = 2,
    HollowBlock = 3,
}

impl CaretShape {
    pub fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Beam,
            2 => Self::Underline,
            3 => Self::HollowBlock,
            _ => Self::Block,
        }
    }
}

/// Pointer cursor hint for the host window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerStyle {
    Default = 0,
    Text = 1,
    Pointer = 2,
    ResizeHorizontal = 3,
    ResizeVertical = 4,
}

impl PointerStyle {
    pub fn from_code(code: i64) -> Self {
        match code {
            1 => Self::Text,
            2 => Self::Pointer,
            3 => Self::ResizeHorizontal,
            4 => Self::ResizeVertical,
            _ => Self::Default,
        }
    }
}

/// Window-level metadata committed alongside a scene frame.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowConfig {
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub background: ColorRgba,
    pub pointer: PointerStyle,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "Lava".to_string(),
            width: 1280,
            height: 800,
            background: ColorRgba::from_rgba(30, 30, 46, 255),
            pointer: PointerStyle::Default,
        }
    }
}

/// Flat 2D drawing command interpreted by the GTK4 / Cairo renderer.
#[derive(Debug, Clone, PartialEq)]
pub enum DrawCmd {
    ClipPush {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
    },
    ClipPop,
    FillRect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        color: ColorRgba,
        radius: f64,
    },
    StrokeRect {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        color: ColorRgba,
        line_width: f64,
        radius: f64,
    },
    Line {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        color: ColorRgba,
        line_width: f64,
    },
    TextRun {
        x: f64,
        y: f64,
        text: String,
        font_family: String,
        font_size_pt: f64,
        color: ColorRgba,
        bold: bool,
        italic: bool,
        underline: bool,
    },
    CursorCaret {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        shape: CaretShape,
        color: ColorRgba,
    },
}

/// Immutable committed render snapshot produced by Nessa and consumed by GTK4.
#[derive(Debug, Clone, Default)]
pub struct RenderFrame {
    pub generation: u64,
    pub window: WindowConfig,
    pub commands: Vec<DrawCmd>,
}

/// Measured font metrics in logical pixels for layout computation in Nessa.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FontMetrics {
    pub cell_width: f64,
    pub cell_height: f64,
    pub ascent: f64,
    pub descent: f64,
}

impl Default for FontMetrics {
    fn default() -> Self {
        Self {
            cell_width: 8.4,
            cell_height: 19.0,
            ascent: 15.0,
            descent: 4.0,
        }
    }
}

/// Mouse button action kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
}

/// Platform UI, timer, and async background completion events delivered to Nessa.
#[derive(Debug, Clone, PartialEq)]
pub enum UiEvent {
    Key {
        key: String,
        modifiers: u32,
        text: Option<String>,
    },
    MouseButton {
        action: MouseAction,
        button: u32,
        x: f64,
        y: f64,
        modifiers: u32,
        press_count: i32,
    },
    MouseMove {
        x: f64,
        y: f64,
        modifiers: u32,
    },
    MouseScroll {
        dx: f64,
        dy: f64,
        x: f64,
        y: f64,
        modifiers: u32,
    },
    Resize {
        width: i32,
        height: i32,
        scale_factor: i32,
    },
    Focus {
        focused: bool,
    },
    CloseRequested,
    Tick {
        timer_id: u64,
        now_ms: i64,
    },
    AsyncJobDone {
        job_id: u64,
        ok: bool,
        exit_code: i32,
        stdout: String,
        stderr: String,
    },
}

/// Thread-safe slot holding the most recently committed [`RenderFrame`].
pub struct FrameSlot {
    inner: Mutex<Arc<RenderFrame>>,
    wakeup: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl Default for FrameSlot {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSlot {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Arc::new(RenderFrame::default())),
            wakeup: Mutex::new(None),
        }
    }

    pub fn set_ui_waker(&self, waker: Box<dyn Fn() + Send + Sync>) {
        if let Ok(mut guard) = self.wakeup.lock() {
            *guard = Some(waker);
        }
    }

    pub fn commit(&self, mut frame: RenderFrame) -> u64 {
        let next_gen = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let next = guard.generation.wrapping_add(1);
            frame.generation = next;
            *guard = Arc::new(frame);
            next
        };
        if let Ok(guard) = self.wakeup.lock()
            && let Some(waker) = guard.as_ref()
        {
            waker();
        }
        next_gen
    }

    pub fn latest(&self) -> Arc<RenderFrame> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&guard)
    }
}

struct EventQueueState {
    queue: VecDeque<UiEvent>,
    closed: bool,
    quit_requested: Option<i32>,
}

/// Thread-safe event mailbox from UI / timer / background workers to the Nessa VM thread.
pub struct EventQueue {
    state: Mutex<EventQueueState>,
    cvar: Condvar,
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl EventQueue {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(EventQueueState {
                queue: VecDeque::new(),
                closed: false,
                quit_requested: None,
            }),
            cvar: Condvar::new(),
        }
    }

    pub fn push(&self, event: UiEvent) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !guard.closed {
            // Coalesce consecutive resize or mouse-move events to keep UI snappy.
            match (&event, guard.queue.back_mut()) {
                (
                    UiEvent::Resize {
                        width,
                        height,
                        scale_factor,
                    },
                    Some(UiEvent::Resize {
                        width: pw,
                        height: ph,
                        scale_factor: ps,
                    }),
                ) => {
                    *pw = *width;
                    *ph = *height;
                    *ps = *scale_factor;
                }
                (
                    UiEvent::MouseMove { x, y, modifiers },
                    Some(UiEvent::MouseMove {
                        x: px,
                        y: py,
                        modifiers: pm,
                    }),
                ) => {
                    *px = *x;
                    *py = *y;
                    *pm = *modifiers;
                }
                _ => guard.queue.push_back(event),
            }
            self.cvar.notify_all();
        }
    }

    pub fn drain(&self) -> Vec<UiEvent> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.queue.drain(..).collect()
    }

    pub fn wait_for_events(&self, timeout: Option<Duration>) -> Vec<UiEvent> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if guard.queue.is_empty() && !guard.closed && guard.quit_requested.is_none() {
            match timeout {
                Some(dur) => {
                    let (g, _) = self
                        .cvar
                        .wait_timeout(guard, dur)
                        .unwrap_or_else(|e| e.into_inner());
                    guard = g;
                }
                None => {
                    guard = self.cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
        guard.queue.drain(..).collect()
    }

    pub fn request_quit(&self, exit_code: i32) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.quit_requested = Some(exit_code);
        guard.closed = true;
        self.cvar.notify_all();
    }

    pub fn quit_code(&self) -> Option<i32> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.quit_requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_packing_and_components_roundtrip() {
        let c = ColorRgba::from_rgba(255, 128, 0, 255);
        assert_eq!(c.0, 0xFF80_00FF);
        let (r, g, b, a) = c.components_f64();
        assert!((r - 1.0).abs() < 1e-6);
        assert!((g - 128.0 / 255.0).abs() < 1e-6);
        assert!(b.abs() < 1e-6);
        assert!((a - 1.0).abs() < 1e-6);
    }

    #[test]
    fn frame_slot_increments_generation_and_invokes_waker() {
        let slot = FrameSlot::new();
        let hits = Arc::new(Mutex::new(0u32));
        let hits_clone = Arc::clone(&hits);
        slot.set_ui_waker(Box::new(move || {
            *hits_clone.lock().unwrap() += 1;
        }));
        let g1 = slot.commit(RenderFrame::default());
        let g2 = slot.commit(RenderFrame::default());
        assert_eq!(g1, 1);
        assert_eq!(g2, 2);
        assert_eq!(slot.latest().generation, 2);
        assert_eq!(*hits.lock().unwrap(), 2);
    }

    #[test]
    fn event_queue_coalesces_resize_and_mouse_move() {
        let q = EventQueue::new();
        q.push(UiEvent::Resize {
            width: 800,
            height: 600,
            scale_factor: 1,
        });
        q.push(UiEvent::Resize {
            width: 1024,
            height: 768,
            scale_factor: 2,
        });
        q.push(UiEvent::MouseMove {
            x: 10.0,
            y: 20.0,
            modifiers: 0,
        });
        q.push(UiEvent::MouseMove {
            x: 30.0,
            y: 40.0,
            modifiers: mods::SHIFT,
        });
        let drained = q.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(
            drained[0],
            UiEvent::Resize {
                width: 1024,
                height: 768,
                scale_factor: 2
            }
        );
        assert_eq!(
            drained[1],
            UiEvent::MouseMove {
                x: 30.0,
                y: 40.0,
                modifiers: mods::SHIFT
            }
        );
    }
}
