//! Dark palette shared by all views.

pub const BACKGROUND: u32 = 0x1e1f22;
pub const PANEL: u32 = 0x2b2d30;
pub const BORDER: u32 = 0x393b40;
pub const TEXT: u32 = 0xbcbec4;
pub const MUTED: u32 = 0x6f737a;
pub const ACCENT: u32 = 0x3574f0;
pub const ACTIVE_ROW: u32 = 0x2e436e;
pub const HOVER_ROW: u32 = 0x35373b;
pub const SELECTION: u32 = 0x214283;
pub const CURSOR: u32 = 0xced0d6;
pub const ERROR: u32 = 0xf75464;

pub const EDITOR_FONT_SIZE: f32 = 14.;
pub const EDITOR_LINE_HEIGHT: f32 = 20.;

#[cfg(target_os = "windows")]
pub const MONO_FONT: &str = "Consolas";
#[cfg(target_os = "macos")]
pub const MONO_FONT: &str = "Menlo";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub const MONO_FONT: &str = "DejaVu Sans Mono";
