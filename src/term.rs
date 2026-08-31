//! Terminal setup and, more importantly, teardown.

use anyhow::Result;
use crossterm::{cursor, execute, terminal};
use std::io::{self, Write};

/// Restores the terminal to a usable state. Safe to call more than once, which
/// matters because both the panic hook and the guard's Drop may run.
pub fn restore() {
    let mut out = io::stdout();
    let _ = execute!(out, cursor::Show, terminal::LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}

/// Owns the terminal mode. Dropping it puts things back; a panic hook covers
/// the case where we never get to the drop.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn new() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prev(info);
        }));
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

pub fn size() -> (u16, u16) {
    terminal::size().unwrap_or((80, 24))
}

/// Terminals advertise 24-bit colour through COLORTERM. Without it we fall back
/// to the 256-colour cube, which every modern terminal handles.
pub fn truecolor() -> bool {
    std::env::var("COLORTERM")
        .map(|v| v.contains("truecolor") || v.contains("24bit"))
        .unwrap_or(false)
}
