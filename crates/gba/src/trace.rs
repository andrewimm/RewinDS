//! A structured, timestamped event trace for bring-up and debugging.
//!
//! The trace records the causal chain that drives the machine — MMIO the CPU
//! writes, the events those writes schedule, the events as they fire, and the
//! interrupts the CPU accepts — each stamped with the guest time it happened.
//! It is opt-in (off by default, so it costs nothing during normal execution)
//! and lives on the [`crate::system::System`], which observes every one of those
//! points as it runs.
//!
//! Example, rendered by [`Trace::to_text`]:
//!
//! ```text
//! 10240  CPU write TM0CNT_H = 0x00c0
//! 10240  scheduled next event @ 26624
//! 26624  Timer0 overflow
//! 26624  CPU accepts IRQ
//! ```

use crate::event::{EventKind, PpuEvent, TimerEvent};
use emu_core::Timestamp;

/// One traced entry: what happened, and when in guest time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceRecord {
    pub at: Timestamp,
    pub message: String,
}

/// A collected sequence of trace records, in the order they occurred.
#[derive(Clone, Debug, Default)]
pub struct Trace {
    records: Vec<TraceRecord>,
}

impl Trace {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, at: Timestamp, message: impl Into<String>) {
        self.records.push(TraceRecord {
            at,
            message: message.into(),
        });
    }

    pub fn records(&self) -> &[TraceRecord] {
        &self.records
    }

    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// Render the trace as aligned `time  message` lines.
    pub fn to_text(&self) -> String {
        self.records
            .iter()
            .map(|record| format!("{:>8}  {}", record.at, record.message))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A short human-readable name for a scheduled event.
pub(crate) fn describe_event(kind: &EventKind) -> String {
    match kind {
        EventKind::Timer(TimerEvent::Overflow { timer, .. }) => format!("{timer:?} overflow"),
        EventKind::Ppu(PpuEvent::HBlank) => "PPU HBlank".to_string(),
        EventKind::Ppu(PpuEvent::LineStart) => "PPU scanline start".to_string(),
        EventKind::Apu(crate::event::ApuEvent::Sample) => "APU sample".to_string(),
    }
}

/// A short name for a memory-mapped register, for the `CPU write ...` records.
pub(crate) fn io_register_name(address: u32) -> String {
    let name = match address & 0x00FF_FFFF {
        0x000 => "DISPCNT",
        0x004 => "DISPSTAT",
        0x006 => "VCOUNT",
        0x100 => "TM0CNT_L",
        0x102 => "TM0CNT_H",
        0x104 => "TM1CNT_L",
        0x106 => "TM1CNT_H",
        0x108 => "TM2CNT_L",
        0x10A => "TM2CNT_H",
        0x10C => "TM3CNT_L",
        0x10E => "TM3CNT_H",
        0x130 => "KEYINPUT",
        0x132 => "KEYCNT",
        0x200 => "IE",
        0x202 => "IF",
        0x204 => "WAITCNT",
        0x208 => "IME",
        0x301 => "HALTCNT",
        offset if (0x0B0..=0x0DF).contains(&offset) => "DMA",
        _ => return format!("[{address:#010x}]"),
    };
    name.to_string()
}
