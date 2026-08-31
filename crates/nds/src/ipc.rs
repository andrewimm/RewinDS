//! Inter-Process Communication between the ARM9 and ARM7.
//!
//! Two mechanisms, both symmetric between the cores:
//!
//! - **IPCSYNC** — a 4-bit value each core presents to the other, plus a manual
//!   "send IRQ to the remote" trigger.
//! - **IPCFIFO** — a 16-word send queue in each direction. A core writes
//!   `IPCFIFOSEND` to enqueue toward the remote and reads `IPCFIFORECV` to dequeue
//!   what the remote sent. Two edge-triggered interrupts report the send FIFO
//!   going empty and the receive FIFO going non-empty.
//!
//! Because the mechanism spans both cores, it owns both directions and takes the
//! two [`Interrupts`] controllers so it can raise an interrupt on either side.
//! Register layouts follow GBATEK's DS IPC section.

use std::collections::VecDeque;

use crate::interrupt::{Interrupts, IrqSource};
use crate::memory::Core;

const FIFO_DEPTH: usize = 16;

/// The shared IPC state for both cores. Per-core arrays are indexed by
/// [`Core::index`]; `fifo[c]` is the queue core `c` **sends into** (the remote
/// receives from it).
#[derive(Default)]
pub struct Ipc {
    /// IPCSYNC data output (bits 8-11) each core presents to the other.
    sync_out: [u8; 2],
    /// IPCSYNC bit 14: accept a remote-triggered sync IRQ.
    sync_irq_enable: [bool; 2],
    fifo: [VecDeque<u32>; 2],
    /// IPCFIFOCNT bit 15: FIFOs enabled.
    fifo_enabled: [bool; 2],
    /// IPCFIFOCNT bit 2: send-empty IRQ enabled.
    send_irq_enable: [bool; 2],
    /// IPCFIFOCNT bit 10: receive-not-empty IRQ enabled.
    recv_irq_enable: [bool; 2],
    /// IPCFIFOCNT bit 14: error (read-empty / send-full) flag.
    error: [bool; 2],
    /// The most recently received word, returned on an empty read.
    last_recv: [u32; 2],
    /// Previous edge-detector conditions, for the two FIFO interrupts.
    prev_send_empty: [bool; 2],
    prev_recv_ready: [bool; 2],
}

/// The remote core's index.
fn remote(core: Core) -> usize {
    1 - core.index()
}

impl Ipc {
    pub fn new() -> Self {
        Ipc::default()
    }

    // --- IPCSYNC ------------------------------------------------------------

    /// Read `IPCSYNC`: the remote's output in bits 0-3, our output in bits 8-11,
    /// our sync-IRQ enable in bit 14.
    pub fn read_sync(&self, core: Core) -> u16 {
        let c = core.index();
        (self.sync_out[remote(core)] as u16)
            | (self.sync_out[c] as u16) << 8
            | (self.sync_irq_enable[c] as u16) << 14
    }

    /// Write `IPCSYNC`: set our output (bits 8-11) and sync-IRQ enable (bit 14);
    /// bit 13 fires an IRQ at the remote if it has enabled one.
    pub fn write_sync(&mut self, core: Core, value: u16, irqs: &mut [Interrupts; 2]) {
        let c = core.index();
        self.sync_out[c] = ((value >> 8) & 0xF) as u8;
        self.sync_irq_enable[c] = value & (1 << 14) != 0;
        if value & (1 << 13) != 0 {
            let r = remote(core);
            if self.sync_irq_enable[r] {
                irqs[r].request(IrqSource::IpcSync);
            }
        }
    }

    // --- IPCFIFOCNT ---------------------------------------------------------

    /// Read `IPCFIFOCNT`: send/receive status plus the enable bits.
    pub fn read_fifocnt(&self, core: Core) -> u16 {
        let c = core.index();
        let send = &self.fifo[c];
        let recv = &self.fifo[remote(core)];
        let mut v = 0u16;
        v |= send.is_empty() as u16;
        v |= ((send.len() >= FIFO_DEPTH) as u16) << 1;
        v |= (self.send_irq_enable[c] as u16) << 2;
        v |= (recv.is_empty() as u16) << 8;
        v |= ((recv.len() >= FIFO_DEPTH) as u16) << 9;
        v |= (self.recv_irq_enable[c] as u16) << 10;
        v |= (self.error[c] as u16) << 14;
        v |= (self.fifo_enabled[c] as u16) << 15;
        v
    }

    /// Write `IPCFIFOCNT`: update the enables, optionally clear the send FIFO
    /// (bit 3) or acknowledge the error (bit 14).
    pub fn write_fifocnt(&mut self, core: Core, value: u16, irqs: &mut [Interrupts; 2]) {
        let c = core.index();
        self.send_irq_enable[c] = value & (1 << 2) != 0;
        self.recv_irq_enable[c] = value & (1 << 10) != 0;
        self.fifo_enabled[c] = value & (1 << 15) != 0;
        if value & (1 << 3) != 0 {
            self.fifo[c].clear();
        }
        if value & (1 << 14) != 0 {
            self.error[c] = false;
        }
        self.update_edges(irqs);
    }

    // --- IPCFIFOSEND / IPCFIFORECV -----------------------------------------

    /// Write `IPCFIFOSEND`: enqueue a word toward the remote. Ignored when FIFOs
    /// are disabled; overflowing a full FIFO raises the error flag.
    pub fn send(&mut self, core: Core, value: u32, irqs: &mut [Interrupts; 2]) {
        let c = core.index();
        if !self.fifo_enabled[c] {
            return;
        }
        if self.fifo[c].len() >= FIFO_DEPTH {
            self.error[c] = true;
        } else {
            self.fifo[c].push_back(value);
        }
        self.update_edges(irqs);
    }

    /// Read `IPCFIFORECV`: dequeue a word the remote sent. An empty read returns
    /// the last word (or zero) and raises the error flag.
    pub fn recv(&mut self, core: Core, irqs: &mut [Interrupts; 2]) -> u32 {
        let r = remote(core);
        let value = if let Some(word) = self.fifo[r].pop_front() {
            self.last_recv[core.index()] = word;
            word
        } else {
            self.error[core.index()] = true;
            self.last_recv[core.index()]
        };
        self.update_edges(irqs);
        value
    }

    /// Recompute the two edge-triggered FIFO interrupt conditions for both cores
    /// and raise on any rising edge. `send-empty` watches a core's own send FIFO;
    /// `recv-ready` watches the FIFO it receives from.
    fn update_edges(&mut self, irqs: &mut [Interrupts; 2]) {
        for (c, irq) in irqs.iter_mut().enumerate() {
            let core = if c == 0 { Core::Arm9 } else { Core::Arm7 };
            let send_empty = self.send_irq_enable[c] && self.fifo[c].is_empty();
            let recv_ready = self.recv_irq_enable[c] && !self.fifo[remote(core)].is_empty();
            if send_empty && !self.prev_send_empty[c] {
                irq.request(IrqSource::IpcSendEmpty);
            }
            if recv_ready && !self.prev_recv_ready[c] {
                irq.request(IrqSource::IpcRecvNotEmpty);
            }
            self.prev_send_empty[c] = send_empty;
            self.prev_recv_ready[c] = recv_ready;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn irqs() -> [Interrupts; 2] {
        [Interrupts::new(), Interrupts::new()]
    }

    #[test]
    fn sync_output_is_visible_to_the_other_core() {
        let mut ipc = Ipc::new();
        let mut irq = irqs();
        ipc.write_sync(Core::Arm9, 0x0A << 8, &mut irq); // ARM9 outputs 0xA
        // The ARM7 reads the ARM9's output in bits 0-3.
        assert_eq!(ipc.read_sync(Core::Arm7) & 0xF, 0x0A);
    }

    #[test]
    fn sync_bit13_raises_irq_only_when_remote_enabled() {
        let mut ipc = Ipc::new();
        let mut irq = irqs();
        // Remote (ARM7) has not enabled the sync IRQ: no request.
        ipc.write_sync(Core::Arm9, 1 << 13, &mut irq);
        assert_eq!(irq[Core::Arm7.index()].iflags(), 0);
        // ARM7 enables it (bit 14), then ARM9 triggers.
        ipc.write_sync(Core::Arm7, 1 << 14, &mut irq);
        ipc.write_sync(Core::Arm9, 1 << 13, &mut irq);
        assert_eq!(irq[Core::Arm7.index()].iflags(), IrqSource::IpcSync.mask());
    }

    #[test]
    fn fifo_sends_one_way_and_reports_status() {
        let mut ipc = Ipc::new();
        let mut irq = irqs();
        // Both cores enable their FIFOs.
        ipc.write_fifocnt(Core::Arm9, 1 << 15, &mut irq);
        ipc.write_fifocnt(Core::Arm7, 1 << 15, &mut irq);
        ipc.send(Core::Arm9, 0xDEAD_BEEF, &mut irq);
        // The ARM7's receive side is now non-empty; the ARM9's send side too.
        assert_eq!(ipc.read_fifocnt(Core::Arm7) & (1 << 8), 0); // recv not empty
        assert_eq!(ipc.recv(Core::Arm7, &mut irq), 0xDEAD_BEEF);
        // Drained: the ARM7 recv side is empty again.
        assert_ne!(ipc.read_fifocnt(Core::Arm7) & (1 << 8), 0);
    }

    #[test]
    fn recv_not_empty_irq_fires_on_the_receiver() {
        let mut ipc = Ipc::new();
        let mut irq = irqs();
        ipc.write_fifocnt(Core::Arm9, 1 << 15, &mut irq); // ARM9 FIFO enable
        // ARM7 enables its receive-not-empty IRQ (bit 10) + FIFO.
        ipc.write_fifocnt(Core::Arm7, (1 << 15) | (1 << 10), &mut irq);
        assert_eq!(irq[Core::Arm7.index()].iflags(), 0);
        ipc.send(Core::Arm9, 0x1, &mut irq); // a word arrives for the ARM7
        assert_eq!(irq[Core::Arm7.index()].iflags(), IrqSource::IpcRecvNotEmpty.mask());
    }

    #[test]
    fn empty_read_sets_the_error_flag() {
        let mut ipc = Ipc::new();
        let mut irq = irqs();
        ipc.write_fifocnt(Core::Arm7, 1 << 15, &mut irq);
        let _ = ipc.recv(Core::Arm7, &mut irq); // nothing sent yet
        assert_ne!(ipc.read_fifocnt(Core::Arm7) & (1 << 14), 0); // error set
    }
}
