//! Log sink setup.
//!
//! The `log` crate is the configurable sink: code emits through its facade
//! (`log::info!`, `log::warn!`, …) and a frontend installs the sink it wants.
//! This desktop build writes to stderr ("the screen"); a wasm build would
//! install a `console.log`-backed sink, and a mobile build one that also feeds
//! an on-screen text overlay. The emulator core never logs directly — only the
//! frontend chooses a sink — so headless/deterministic runs stay clean.

use log::{Metadata, Record};

/// The desktop sink: one line per record on stderr.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        eprintln!("[{}] {}", record.level(), record.args());
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

/// Install the stderr sink. Call once at startup; a no-op if a sink is already
/// installed.
pub fn init() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}
