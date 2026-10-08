//! Dark palette shared by all views.

use syntax::Highlight;

pub const BACKGROUND: u32 = 0x1e1f22;
pub const PANEL: u32 = 0x2b2d30;
pub const BORDER: u32 = 0x393b40;
pub const TEXT: u32 = 0xbcbec4;
pub const MUTED: u32 = 0x6f737a;
pub const ACCENT: u32 = 0x3574f0;
pub const ACCENT_TEXT: u32 = 0x6ca2ff;
pub const WARNING: u32 = 0xf2c55c;
pub const ACTIVE_ROW: u32 = 0x2e436e;
pub const HOVER_ROW: u32 = 0x35373b;
pub const SELECTION: u32 = 0x214283;
pub const CURSOR: u32 = 0xced0d6;
pub const ERROR: u32 = 0xf75464;

pub fn highlight_color(highlight: Highlight) -> u32 {
    match highlight {
        Highlight::Keyword => 0xcf8e6d,
        Highlight::String => 0x6aab73,
        Highlight::Escape => 0xcf8e6d,
        Highlight::Comment => 0x7a7e85,
        Highlight::DocComment => 0x5f826b,
        Highlight::Number => 0x2aacb8,
        Highlight::Constant => 0xc77dbb,
        Highlight::Function => 0x56a8f5,
        Highlight::Macro => 0xb3ae60,
        Highlight::Type => 0x6fafbd,
        Highlight::Property => 0xc77dbb,
        Highlight::Attribute => 0xb3ae60,
    }
}

pub const EDITOR_FONT_SIZE: f32 = 14.;
pub const EDITOR_LINE_HEIGHT: f32 = 20.;

#[cfg(target_os = "windows")]
pub const MONO_FONT: &str = "Consolas";
#[cfg(target_os = "macos")]
pub const MONO_FONT: &str = "Menlo";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub const MONO_FONT: &str = "DejaVu Sans Mono";

/// gpui alias for the platform UI font.
pub const UI_FONT: &str = ".SystemUIFont";
