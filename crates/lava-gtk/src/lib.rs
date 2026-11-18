//! Native 2D UI & Font Rendering Engine for Lava:
//! - Unix/Linux: GTK4 & PangoCairo (`app`, `render`, `anim`, `keymap`, `ffi`)
//! - Windows (10 1709+): Native Win32 + GDI DIB + Private Bundled TrueType Fonts (`win_*`)

#[cfg(unix)]
mod anim;
#[cfg(unix)]
mod app;
#[cfg(unix)]
mod ffi;
#[cfg(unix)]
mod keymap;
#[cfg(unix)]
mod render;

#[cfg(windows)]
mod win_app;
#[cfg(windows)]
mod win_ffi;
#[cfg(windows)]
mod win_keymap;
#[cfg(windows)]
mod win_render;

pub use lava_proto;

#[cfg(unix)]
pub use app::{
    GtkAppConfig, get_system_clipboard, is_display_available, run_gtk_app,
    run_gtk_app_with_final_size, set_system_clipboard,
};
#[cfg(unix)]
pub use render::{measure_monospace_font, render_frame_to_png};

#[cfg(windows)]
pub use win_app::{
    GtkAppConfig, get_system_clipboard, is_display_available, run_gtk_app,
    run_gtk_app_with_final_size, set_system_clipboard,
};
#[cfg(windows)]
pub use win_render::{
    ensure_bundled_fonts_registered, measure_monospace_font, render_frame_to_png,
};
