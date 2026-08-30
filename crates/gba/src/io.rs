//! The memory-mapped I/O subsystem (`04000000h` region).
//!
//! The bus decodes an address to a region; when that region is I/O, it hands off
//! here. Registers are dispatched at 16-bit granularity — most GBA registers are
//! 16-bit — with a write *mask* so that 8-, 16-, and 32-bit accesses all compose
//! correctly over read-only bits, write-1-to-clear bits (`IF`), and unused bits.
//!
//! Only the registers backed by a modeled device are wired up; other I/O reads
//! as zero and ignores writes for now.

use crate::dma::Dma;
use crate::event::EventKind;
use crate::interrupt::{InterruptController, IrqSource};
use crate::ppu::Ppu;
use crate::timer::{TimerId, Timers};
use emu_core::{AccessWidth, Scheduler, Timestamp};

/// The CPU power state, set via `HALTCNT`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PowerState {
    #[default]
    Running,
    /// Halt: paused until any enabled interrupt is pending (`IE & IF`).
    Halted,
    /// Stop: woken only by keypad, gamepak, or serial interrupts.
    Stopped,
}

/// Interrupt sources that terminate Stop mode.
const STOP_WAKE_MASK: u16 =
    IrqSource::Keypad.mask() | IrqSource::GamePak.mask() | IrqSource::Serial.mask();

/// System-control registers: `HALTCNT` power state, `WAITCNT`, `POSTFLG`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemControl {
    power: PowerState,
    waitcnt: u16,
    postflg: u8,
}

impl SystemControl {
    pub fn power(&self) -> PowerState {
        self.power
    }

    /// `WAITCNT` (`4000204h`) — the gamepak wait-state configuration.
    pub fn waitcnt(&self) -> u16 {
        self.waitcnt
    }

    /// Write `HALTCNT`: bit 7 selects Halt (0) or Stop (1).
    pub fn write_haltcnt(&mut self, value: u8) {
        self.power = if value & 0x80 != 0 {
            PowerState::Stopped
        } else {
            PowerState::Halted
        };
    }

    /// Resume normal execution.
    pub fn wake(&mut self) {
        self.power = PowerState::Running;
    }
}

/// The memory-mapped devices.
#[derive(Clone, Copy, Debug, Default)]
pub struct Io {
    pub control: SystemControl,
    pub irq: InterruptController,
    pub timers: Timers,
    pub dma: Dma,
    pub video: Ppu,
}

impl Io {
    pub fn new() -> Self {
        Self::default()
    }

    // --- power / halt ---

    pub fn power_state(&self) -> PowerState {
        self.control.power()
    }

    pub fn is_low_power(&self) -> bool {
        self.control.power() != PowerState::Running
    }

    /// Whether a pending interrupt should wake the CPU from low power.
    pub fn should_wake(&self) -> bool {
        match self.control.power() {
            PowerState::Running => false,
            PowerState::Halted => self.irq.pending(),
            PowerState::Stopped => self.irq.pending_within(STOP_WAKE_MASK),
        }
    }

    pub fn wake(&mut self) {
        self.control.wake();
    }

    // --- MMIO access, composed from 16-bit registers ---

    /// Read `width` bytes of I/O at `offset` (relative to `04000000h`).
    pub fn read(&self, offset: u32, width: AccessWidth, now: Timestamp) -> u32 {
        match width {
            AccessWidth::Byte => {
                let shift = 8 * (offset & 1);
                ((self.read16(offset & !1, now) >> shift) & 0xFF) as u32
            }
            AccessWidth::Half => self.read16(offset, now) as u32,
            AccessWidth::Word => {
                self.read16(offset, now) as u32 | ((self.read16(offset + 2, now) as u32) << 16)
            }
        }
    }

    /// Write `width` bytes of I/O at `offset`. Returns whether the scheduler's
    /// next deadline may have changed.
    pub fn write(
        &mut self,
        offset: u32,
        width: AccessWidth,
        value: u32,
        scheduler: &mut Scheduler<EventKind>,
    ) -> bool {
        match width {
            AccessWidth::Byte => {
                let shift = 8 * (offset & 1);
                self.write16(
                    offset & !1,
                    ((value & 0xFF) << shift) as u16,
                    (0xFFu16) << shift,
                    scheduler,
                )
            }
            AccessWidth::Half => self.write16(offset, value as u16, 0xFFFF, scheduler),
            AccessWidth::Word => {
                let low = self.write16(offset, value as u16, 0xFFFF, scheduler);
                let high = self.write16(offset + 2, (value >> 16) as u16, 0xFFFF, scheduler);
                low || high
            }
        }
    }

    fn read16(&self, offset: u32, now: Timestamp) -> u16 {
        match offset {
            0x000 => self.video.read_dispcnt(),
            0x004 => self.video.read_dispstat(),
            0x006 => self.video.read_vcount(),
            0x0B0..=0x0DF => self.dma.read_register(offset),
            0x100 | 0x104 | 0x108 | 0x10C => {
                self.timers.read_counter(timer_at(offset, 0x100), now)
            }
            0x102 | 0x106 | 0x10A | 0x10E => self.timers.read_control(timer_at(offset, 0x102)),
            0x200 => self.irq.ie(),
            0x202 => self.irq.iflags(),
            0x204 => self.control.waitcnt(),
            0x208 => self.irq.ime() as u16,
            // POSTFLG (low byte); HALTCNT (high byte) is write-only.
            0x300 => self.control.postflg as u16,
            _ => 0,
        }
    }

    /// Apply a masked 16-bit write to the register at `offset`. Returns whether
    /// scheduling may have changed.
    fn write16(&mut self, offset: u32, value: u16, mask: u16, scheduler: &mut Scheduler<EventKind>) -> bool {
        match offset {
            0x000 => {
                let merged = merge(self.video.read_dispcnt(), value, mask);
                self.video.write_dispcnt(merged);
                false
            }
            0x004 => {
                let merged = merge(self.video.read_dispstat(), value, mask);
                self.video.write_dispstat(merged);
                false
            }
            // 0x006 VCOUNT is read-only.
            0x0B0..=0x0DF => {
                self.dma.write_register(offset, value, mask);
                // The transfer itself is run by the bus after this returns.
                false
            }
            0x100 | 0x104 | 0x108 | 0x10C => {
                let id = timer_at(offset, 0x100);
                let merged = merge(self.timers.read_reload(id), value, mask);
                self.timers.write_reload(id, merged);
                false
            }
            0x102 | 0x106 | 0x10A | 0x10E => {
                let id = timer_at(offset, 0x102);
                let merged = merge(self.timers.read_control(id), value, mask);
                self.timers.write_control(id, merged, scheduler.now(), scheduler);
                // A timer control write can (re)schedule an overflow.
                true
            }
            0x200 => {
                self.irq.set_ie(merge(self.irq.ie(), value, mask));
                false
            }
            // IF is write-1-to-clear: masking keeps a partial-width write from
            // touching the other byte.
            0x202 => {
                self.irq.acknowledge(value & mask);
                false
            }
            0x204 => {
                self.control.waitcnt = merge(self.control.waitcnt, value, mask);
                false
            }
            0x208 => {
                if mask & 1 != 0 {
                    self.irq.set_ime(value & 1 != 0);
                }
                false
            }
            0x300 => {
                if mask & 0x00FF != 0 {
                    self.control.postflg = (value & 0xFF) as u8;
                }
                if mask & 0xFF00 != 0 {
                    self.control.write_haltcnt((value >> 8) as u8);
                }
                false
            }
            _ => false,
        }
    }
}

/// Merge a masked write into a register's current value.
fn merge(current: u16, value: u16, mask: u16) -> u16 {
    (current & !mask) | (value & mask)
}

/// The timer whose register block begins at `base` and is addressed by `offset`
/// (blocks are 4 bytes apart).
fn timer_at(offset: u32, base: u32) -> TimerId {
    TimerId::from_index(((offset - base) / 4) as usize)
}
