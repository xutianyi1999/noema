//! Shared terminal styling for the server transcript and the CLI client.
//!
//! Both surfaces embed anstyle escape codes through anstream writers, which
//! strip the codes when the stream is not a terminal (NO_COLOR /
//! FORCE_COLOR / CLICOLOR honoured). Centralising the palette and the
//! writers here keeps the two surfaces visually identical and the
//! anstyle/anstream dependency in one place.

use anstyle::{AnsiColor, Style};

pub const DIM: Style = Style::new().dimmed();
pub const BOLD: Style = Style::new().bold();
pub const RED: Style = AnsiColor::Red.on_default();
pub const GREEN: Style = AnsiColor::Green.on_default();
pub const YELLOW: Style = AnsiColor::Yellow.on_default();
pub const CYAN: Style = AnsiColor::Cyan.on_default();
pub const MAGENTA: Style = AnsiColor::Magenta.on_default();

/// Embed one style's escape codes around `text`. The [`stdout`]/[`stderr`]
/// writers strip the codes when colors are not appropriate.
pub fn paint(style: Style, text: &str) -> String {
    format!("{style}{text}{style:#}")
}

/// Stdout writer for the CLI client.
pub fn stdout() -> anstream::AutoStream<std::io::Stdout> {
    anstream::AutoStream::auto(std::io::stdout())
}

/// Stderr writer for the server transcript.
pub fn stderr() -> anstream::AutoStream<std::io::Stderr> {
    anstream::AutoStream::auto(std::io::stderr())
}
