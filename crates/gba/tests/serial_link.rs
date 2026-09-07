//! End-to-end serial link: two GBA buses exchanging a Multi-Player round through
//! the real MMIO routing, with frames relayed directly (an in-process carrier).

use emu_core::{Access, AccessKind, AccessSequence, Scheduler};
use gba::{Bus, EventKind, IrqSource};

const CPU: Access = Access::cpu(AccessKind::Data, AccessSequence::NonSequential);
const MULTI: u16 = (0b10 << 12) | (1 << 14); // multiplayer mode + transfer IRQ enable
const START: u16 = 1 << 7;

fn sched() -> Scheduler<EventKind> {
    Scheduler::new()
}

#[test]
fn two_buses_exchange_a_multiplayer_round() {
    let (mut parent, mut child) = (Bus::new(), Bus::new());
    let (mut ps, mut cs) = (sched(), sched());
    parent.io.serial_set_link(true, 0, 2);
    child.io.serial_set_link(true, 1, 2);

    // Serial mode via RCNT, each unit's send word, multiplayer mode in SIOCNT.
    parent.write16(0x0400_0134, 0, CPU, &mut ps);
    child.write16(0x0400_0134, 0, CPU, &mut cs);
    parent.write16(0x0400_012A, 0x1234, CPU, &mut ps); // SIOMLT_SEND
    child.write16(0x0400_012A, 0xABCD, CPU, &mut cs);
    child.write16(0x0400_0128, MULTI, CPU, &mut cs); // child ready, waits for master

    // Parent starts the round; relay frames until both sides settle.
    parent.write16(0x0400_0128, MULTI | START, CPU, &mut ps);
    for _ in 0..8 {
        if let Some(f) = parent.io.serial_poll_out() {
            child.io.serial_deliver(&f);
        }
        if let Some(f) = child.io.serial_poll_out() {
            parent.io.serial_deliver(&f);
        }
    }

    // Both units hold identical SIOMULTI0/1 = {parent word, child word}.
    for bus in [&mut parent, &mut child] {
        let mut s = sched();
        assert_eq!(bus.read16(0x0400_0120, CPU, &mut s).value, 0x1234);
        assert_eq!(bus.read16(0x0400_0122, CPU, &mut s).value, 0xABCD);
        assert_eq!(bus.read16(0x0400_0128, CPU, &mut s).value & START, 0); // busy cleared
    }
    assert_ne!(parent.io.irq.iflags() & IrqSource::Serial.mask(), 0);
    assert_ne!(child.io.irq.iflags() & IrqSource::Serial.mask(), 0);
}
