//! Serial I/O (SIO) — Normal and Multi-Player link modes.
//!
//! The emulator core models the SIO registers, the transfer state machine, and
//! the completion interrupt, but is **carrier-blind**: it never knows how link
//! frames reach the other units. A started transfer produces an opaque
//! [`LinkFrame`] (as bytes) that the host relays over whatever carrier it likes
//! (TCP, Bonjour, WebRTC, …); the host delivers peers' frames back the same way.
//!
//! ## Wire protocol
//!
//! One frame per unit per transfer round, 8 bytes little-endian:
//!   `round: u16, id: u8, mode: u8, word: u32`
//! The master (parent, `id == 0`) starts round `R` by writing the SIOCNT start
//! bit; children join round `R` when they first see any frame for it. Each unit
//! collects one word per participant (its slot indexed by `id`); when all
//! `count` slots are filled the round completes: the received words land in the
//! SIOMULTI/SIODATA registers, the busy bit clears, and the serial IRQ fires.
//! The transfer's busy-wait is itself the sync barrier, so no separate lockstep
//! pacing is needed — the guest simply spins on the busy bit until peers deliver.
//!
//! ## Disconnected fallback
//!
//! With no carrier attached (`connected == false`) a started transfer completes
//! immediately as "nothing connected" (received data all-ones, error bit in
//! multiplayer), so link/adapter detection times out and games boot single-player.

use crate::interrupt::{InterruptController, IrqSource};

/// `SIOCNT` bit 7 — start / busy.
const START_BUSY: u16 = 1 << 7;
/// `SIOCNT` bit 6 — multiplayer error (no other GBA/adapter responded).
const MULTI_ERROR: u16 = 1 << 6;
/// `SIOCNT` bit 14 — transfer-complete interrupt enable.
const IRQ_ENABLE: u16 = 1 << 14;

/// Bytes in one serialized [`LinkFrame`].
pub const LINK_FRAME_LEN: usize = 8;

/// The SIO transfer mode selected by RCNT bit 15 and SIOCNT bits 12-13.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SioMode {
    Normal8,
    Normal32,
    Multiplayer,
    /// UART / JOY-Bus / General-Purpose — not modeled for linking.
    Other,
}

/// One unit's contribution to a transfer round (the carrier-agnostic wire unit).
#[derive(Clone, Copy, Debug)]
struct LinkFrame {
    round: u16,
    id: u8,
    mode: u8,
    word: u32,
}

impl LinkFrame {
    fn to_bytes(self) -> [u8; LINK_FRAME_LEN] {
        let mut b = [0u8; LINK_FRAME_LEN];
        b[0..2].copy_from_slice(&self.round.to_le_bytes());
        b[2] = self.id;
        b[3] = self.mode;
        b[4..8].copy_from_slice(&self.word.to_le_bytes());
        b
    }

    fn from_bytes(b: &[u8]) -> Option<LinkFrame> {
        if b.len() < LINK_FRAME_LEN {
            return None;
        }
        Some(LinkFrame {
            round: u16::from_le_bytes([b[0], b[1]]),
            id: b[2],
            mode: b[3],
            word: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        })
    }
}

/// The serial registers and transfer state.
#[derive(Clone, Debug, Default)]
pub struct Serial {
    siocnt: u16,
    rcnt: u16,
    /// `SIOMULTI0..3` / `SIODATA32` — received data.
    data: [u16; 4],
    /// `SIOMLT_SEND` / `SIODATA8` — outgoing data.
    send: u16,

    /// Whether a carrier is attached. `false` → disconnected fallback.
    connected: bool,
    /// This unit's multiplayer id (0 = parent/master, 1-3 = child).
    id: u8,
    /// Number of linked units (2-4).
    count: u8,
    /// The transfer round currently being assembled.
    round: u16,
    /// Words collected this round, indexed by sender id.
    slots: [Option<u32>; 4],
    /// A frame queued for the host to transmit (one-shot, taken by `poll_out`).
    outbound: Option<LinkFrame>,
}

impl Serial {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a serial register (offsets relative to `0x4000000`).
    pub fn read16(&self, offset: u32) -> u16 {
        match offset {
            0x120 => self.data[0],
            0x122 => self.data[1],
            0x124 => self.data[2],
            0x126 => self.data[3],
            0x128 => self.siocnt_read(),
            0x12A => self.send,
            0x134 => self.rcnt,
            _ => 0,
        }
    }

    /// SIOCNT with the read-only status bits synthesized from the link state.
    fn siocnt_read(&self) -> u16 {
        let mut v = self.siocnt;
        if self.connected && self.mode() == SioMode::Multiplayer {
            v |= 1 << 3; // SD: all connected GBAs are ready
            if self.id != 0 {
                v |= 1 << 2; // SI: this is a child (parent's SI stays low)
            }
            v = (v & !(0b11 << 4)) | ((self.id as u16 & 0b11) << 4); // ID bits
        }
        v
    }

    /// Apply a masked write. Writing the SIOCNT start bit begins a transfer.
    pub fn write16(&mut self, offset: u32, value: u16, mask: u16, irq: &mut InterruptController) {
        let merge = |cur: u16| (cur & !mask) | (value & mask);
        match offset {
            0x120 => self.data[0] = merge(self.data[0]),
            0x122 => self.data[1] = merge(self.data[1]),
            0x124 => self.data[2] = merge(self.data[2]),
            0x126 => self.data[3] = merge(self.data[3]),
            0x128 => {
                let started = merge(self.siocnt) & START_BUSY != 0 && self.siocnt & START_BUSY == 0;
                self.siocnt = merge(self.siocnt);
                if started {
                    self.on_start(irq);
                }
            }
            0x12A => self.send = merge(self.send),
            0x134 => self.rcnt = merge(self.rcnt),
            _ => {}
        }
    }

    fn mode(&self) -> SioMode {
        if self.rcnt & (1 << 15) != 0 {
            return SioMode::Other; // General-Purpose / JOY-Bus
        }
        match (self.siocnt >> 12) & 0b11 {
            0b00 => SioMode::Normal8,
            0b01 => SioMode::Normal32,
            0b10 => SioMode::Multiplayer,
            _ => SioMode::Other, // UART
        }
    }

    /// The word this unit contributes to a round, per mode.
    fn tx_word(&self) -> u32 {
        match self.mode() {
            SioMode::Normal32 => (self.data[0] as u32) | ((self.data[1] as u32) << 16),
            _ => self.send as u32,
        }
    }

    /// Handle a guest write of the SIOCNT start bit.
    fn on_start(&mut self, irq: &mut InterruptController) {
        if !self.connected || self.count < 2 {
            self.disconnected_fallback(irq);
            return;
        }
        if self.id == 0 {
            // Parent/master drives a fresh round.
            self.begin_round(self.round.wrapping_add(1), irq);
        }
        // A child's start bit is effectively read-only: it waits (busy stays set)
        // until the master's round frame arrives via `deliver`.
    }

    /// Complete instantly as "nothing connected" (link/adapter detection path).
    fn disconnected_fallback(&mut self, irq: &mut InterruptController) {
        self.data = [0xFFFF; 4];
        self.siocnt &= !START_BUSY;
        if self.mode() == SioMode::Multiplayer {
            self.siocnt |= MULTI_ERROR;
        }
        if self.siocnt & IRQ_ENABLE != 0 {
            irq.request(IrqSource::Serial);
        }
    }

    /// Join transfer round `r`: latch our word, queue our frame, mark busy.
    fn begin_round(&mut self, r: u16, irq: &mut InterruptController) {
        self.round = r;
        self.slots = [None; 4];
        self.data = [0xFFFF; 4]; // SIOMULTI resets to FFFFh on transfer start
        self.siocnt |= START_BUSY;
        let word = self.tx_word();
        self.slots[self.id as usize] = Some(word);
        self.outbound = Some(LinkFrame { round: r, id: self.id, mode: self.mode() as u8, word });
        if self.filled() >= self.count {
            self.complete_round(irq);
        }
    }

    fn filled(&self) -> u8 {
        self.slots[..self.count.min(4) as usize].iter().filter(|s| s.is_some()).count() as u8
    }

    /// Land the collected words in the data registers, clear busy, raise the IRQ.
    fn complete_round(&mut self, irq: &mut InterruptController) {
        if self.siocnt & START_BUSY == 0 {
            return;
        }
        match self.mode() {
            SioMode::Multiplayer => {
                for i in 0..4 {
                    self.data[i] = self.slots[i].unwrap_or(0xFFFF) as u16;
                }
                // ID bits become valid after the first transfer.
                self.siocnt = (self.siocnt & !(0b11 << 4)) | ((self.id as u16 & 0b11) << 4);
            }
            SioMode::Normal8 => {
                let peer = (1 - self.id as usize).min(3);
                self.send = (self.slots[peer].unwrap_or(0xFFFF) & 0xFF) as u16;
            }
            SioMode::Normal32 => {
                let peer = (1 - self.id as usize).min(3);
                let w = self.slots[peer].unwrap_or(0xFFFF_FFFF);
                self.data[0] = w as u16;
                self.data[1] = (w >> 16) as u16;
            }
            SioMode::Other => {}
        }
        self.siocnt &= !START_BUSY;
        // Do NOT drop `outbound` here: our contribution still needs to reach the
        // peers even though we have already assembled the round locally. It is
        // taken by `poll_out` and overwritten by the next round.
        if self.siocnt & IRQ_ENABLE != 0 {
            irq.request(IrqSource::Serial);
        }
    }

    // --- Carrier-facing API (driven by the host each frame) ------------------

    /// Set the link topology: whether a carrier is attached, this unit's id
    /// (0 = parent), and the number of linked units. Discrete, host-driven.
    pub fn set_link(&mut self, connected: bool, id: u8, count: u8) {
        self.connected = connected;
        self.id = id.min(3);
        self.count = count.min(4);
        if !connected {
            self.slots = [None; 4];
            self.outbound = None;
            self.round = 0;
        }
    }

    /// Take the frame queued for transmission, encoded for the carrier.
    pub fn poll_out(&mut self) -> Option<[u8; LINK_FRAME_LEN]> {
        self.outbound.take().map(LinkFrame::to_bytes)
    }

    /// Deliver a peer's frame received from the carrier.
    pub fn deliver(&mut self, bytes: &[u8], irq: &mut InterruptController) {
        if !self.connected {
            return;
        }
        let Some(frame) = LinkFrame::from_bytes(bytes) else {
            return;
        };
        if (frame.id as usize) >= 4 {
            return;
        }
        // First frame of a not-yet-joined round: join it (also queues our reply).
        if frame.round != self.round || self.slots[self.id as usize].is_none() {
            self.begin_round(frame.round, irq);
        }
        if frame.round == self.round {
            self.slots[frame.id as usize] = Some(frame.word);
            if self.filled() >= self.count {
                self.complete_round(irq);
            }
        }
    }

    /// Whether a transfer is in progress (busy), i.e. the host may have a frame
    /// to pump and should keep relaying.
    pub fn pending(&self) -> bool {
        self.outbound.is_some() || self.siocnt & START_BUSY != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI: u16 = 0b10 << 12; // SIOCNT multiplayer mode bits

    #[test]
    fn disconnected_transfer_completes_and_fires_irq() {
        let mut serial = Serial::new();
        let mut irq = InterruptController::new();
        serial.write16(0x128, START_BUSY | IRQ_ENABLE, 0xFFFF, &mut irq);
        assert_eq!(serial.read16(0x128) & START_BUSY, 0);
        assert_eq!(serial.read16(0x120), 0xFFFF);
        assert_ne!(irq.iflags() & IrqSource::Serial.mask(), 0);
    }

    #[test]
    fn no_irq_without_enable() {
        let mut serial = Serial::new();
        let mut irq = InterruptController::new();
        serial.write16(0x128, START_BUSY, 0xFFFF, &mut irq);
        assert_eq!(serial.read16(0x128) & START_BUSY, 0);
        assert_eq!(irq.iflags() & IrqSource::Serial.mask(), 0);
    }

    /// Two connected units exchanging one multiplayer round, frames relayed
    /// directly between them (the carrier is a no-op in-process relay here).
    #[test]
    fn multiplayer_round_exchanges_words_between_two_units() {
        let mut parent = Serial::new();
        let mut child = Serial::new();
        let mut pirq = InterruptController::new();
        let mut cirq = InterruptController::new();
        parent.set_link(true, 0, 2);
        child.set_link(true, 1, 2);

        // Each unit sets its send word and multiplayer mode.
        parent.write16(0x134, 0, 0xFFFF, &mut pirq); // RCNT: serial mode
        child.write16(0x134, 0, 0xFFFF, &mut cirq);
        parent.write16(0x128, MULTI, 0xFFFF, &mut pirq);
        child.write16(0x128, MULTI, 0xFFFF, &mut cirq);
        parent.write16(0x12A, 0xAAAA, 0xFFFF, &mut pirq); // SIOMLT_SEND
        child.write16(0x12A, 0x5555, 0xFFFF, &mut cirq);

        // Parent starts; relay frames until both complete.
        parent.write16(0x128, MULTI | START_BUSY | IRQ_ENABLE, 0xFFFF, &mut pirq);
        child.write16(0x128, MULTI | IRQ_ENABLE, 0xFFFF, &mut cirq); // child "ready"

        // Pump frames back and forth until neither has outbound data.
        for _ in 0..8 {
            if let Some(f) = parent.poll_out() {
                child.deliver(&f, &mut cirq);
            }
            if let Some(f) = child.poll_out() {
                parent.deliver(&f, &mut pirq);
            }
        }

        // Both units now hold identical SIOMULTI0/1 = {parent word, child word}.
        assert_eq!(parent.read16(0x120), 0xAAAA);
        assert_eq!(parent.read16(0x122), 0x5555);
        assert_eq!(child.read16(0x120), 0xAAAA);
        assert_eq!(child.read16(0x122), 0x5555);
        // Both cleared busy and requested the serial IRQ.
        assert_eq!(parent.read16(0x128) & START_BUSY, 0);
        assert_eq!(child.read16(0x128) & START_BUSY, 0);
        assert_ne!(pirq.iflags() & IrqSource::Serial.mask(), 0);
        assert_ne!(cirq.iflags() & IrqSource::Serial.mask(), 0);
        // The child reads its ID bits as 1.
        assert_eq!((child.read16(0x128) >> 4) & 0b11, 1);
    }
}
