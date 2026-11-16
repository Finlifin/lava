//! GTK4 & PangoCairo native UI engine for Lava.
//!
//! Implements a flat 2D rendering surface, offscreen font measurement and PNG export, GDK4 key
//! translation, and a dedicated native GTK4 event loop decoupled from the Nessa VM thread.

mod app;
mod ffi;
mod keymap;
mod render;

pub use lava_proto;

pub use app::{
    GtkAppConfig, get_system_clipboard, is_display_available, run_gtk_app, set_system_clipboard,
};
pub use render::{measure_monospace_font, render_frame_to_png};
