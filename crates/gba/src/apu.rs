//! The audio processing unit — DirectSound (FIFO) mixing.
//!
//! The GBA mixes six channels: four Game Boy PSG channels and two 8-bit PCM
//! "DirectSound" channels (A and B) fed from a 32-byte FIFO. Commercial games
//! (the MP2K/Sappy engine used by Pokémon, Advance Wars, Fire Emblem, …) do all
//! their mixing in software and stream the result through the DirectSound FIFOs,
//! so those are modeled first; the PSG channels are stubbed for readback and
//! synthesized later.
//!
//! Each DirectSound FIFO is clocked by Timer 0 or 1 (selected in `SOUNDCNT_H`):
//! on the selected timer's overflow one byte is popped and latched as the
//! channel's current output, and when the FIFO runs low a DMA refills it. The
//! mixer samples the latched values at the output rate (a scheduler event),
//! appending interleaved stereo `i16` frames to an output buffer the host drains.

use crate::psg::Psg;
use emu_core::Timestamp;

/// Emitted output sample rate. The CPU runs at 2^24 Hz, so one sample every 512
/// cycles is 32768 Hz — the GBA's typical `SOUNDBIAS` output rate.
pub const SAMPLE_RATE: u32 = 32_768;
/// PSG base cycles spanned by one output sample (2^22 Hz / 32768).
const PSG_CYCLES_PER_SAMPLE: i32 = 128;

/// Mix gains, chosen to bring DirectSound and the PSG to comparable per-channel
/// loudness (matching hardware, where a full DirectSound channel and a full PSG
/// channel are similar) at the natural register levels. `DS_GAIN` keeps
/// DirectSound at the level that already sounded right.
const DS_GAIN: i32 = 48;
const PSG_GAIN: i32 = 96;
/// DC-blocking high-pass coefficient: cutoff ≈ 4 Hz at 32768 Hz, removing the
/// offset the PSG's unipolar output (and any DirectSound bias) would add.
const DC_BLOCK_R: f64 = 0.9992;
/// One-pole output low-pass coefficient: cutoff ≈ 8.4 kHz at 32768 Hz. Stands in
/// for the GBA's analog output filter, shaving the harsh high-frequency imaging
/// our zero-order-hold resampling produces.
const LOW_PASS_ALPHA: f64 = 0.80;
/// CPU cycles between emitted samples.
pub const CYCLES_PER_SAMPLE: Timestamp = 16_777_216 / SAMPLE_RATE as u64;

/// FIFO capacity in bytes.
const FIFO_SIZE: usize = 32;
/// A DMA refill is requested once the FIFO has this many bytes or fewer.
const FIFO_REFILL_THRESHOLD: usize = 16;

// SOUNDCNT_H (0x082) bits.
const H_DSA_VOLUME_FULL: u16 = 1 << 2; // 0 = 50%, 1 = 100%
const H_DSB_VOLUME_FULL: u16 = 1 << 3;
const H_DSA_ENABLE_RIGHT: u16 = 1 << 8;
const H_DSA_ENABLE_LEFT: u16 = 1 << 9;
const H_DSA_TIMER: u16 = 1 << 10; // 0 = Timer 0, 1 = Timer 1
const H_DSA_RESET: u16 = 1 << 11;
const H_DSB_ENABLE_RIGHT: u16 = 1 << 12;
const H_DSB_ENABLE_LEFT: u16 = 1 << 13;
const H_DSB_TIMER: u16 = 1 << 14;
const H_DSB_RESET: u16 = 1 << 15;
// SOUNDCNT_X (0x084) bit 7: master enable.
const X_MASTER_ENABLE: u16 = 1 << 7;

/// A DirectSound FIFO: a 32-byte ring of signed 8-bit samples.
#[derive(Clone, Debug)]
struct Fifo {
    data: [i8; FIFO_SIZE],
    read: usize,
    len: usize,
}

impl Default for Fifo {
    fn default() -> Self {
        Fifo { data: [0; FIFO_SIZE], read: 0, len: 0 }
    }
}

impl Fifo {
    fn clear(&mut self) {
        self.read = 0;
        self.len = 0;
    }

    /// Push a 32-bit word (four little-endian samples) from a FIFO write.
    fn push_word(&mut self, word: u32) {
        for i in 0..4 {
            if self.len >= FIFO_SIZE {
                break; // overflow: extra bytes are dropped
            }
            let idx = (self.read + self.len) % FIFO_SIZE;
            self.data[idx] = (word >> (8 * i)) as i8;
            self.len += 1;
        }
    }

    /// Pop one sample, or return 0 (and stay empty) when starved.
    fn pop(&mut self) -> i8 {
        if self.len == 0 {
            return 0;
        }
        let sample = self.data[self.read];
        self.read = (self.read + 1) % FIFO_SIZE;
        self.len -= 1;
        sample
    }
}

/// The audio processing unit.
#[derive(Clone, Debug)]
pub struct Apu {
    /// DirectSound/bias control registers `0x082..0x090` (readback + synth source).
    regs: [u8; 0x30],
    /// The four Game Boy PSG channels (registers `0x060..0x082` + wave RAM).
    psg: Psg,
    fifo: [Fifo; 2],
    /// The latched current output of each DirectSound channel (updated on the
    /// channel's timer overflow, held between overflows).
    latch: [i8; 2],
    /// DC-blocking filter state (previous input/output) per output channel.
    dc_x: [f64; 2],
    dc_y: [f64; 2],
    /// Output low-pass filter state per channel, and whether it is engaged.
    lp_y: [f64; 2],
    low_pass_on: bool,
    /// Debug mutes for isolating the mix; the channels still advance so
    /// unmuting doesn't glitch.
    mute_directsound: bool,
    mute_psg: bool,
    /// Clip metering since the last drain: how many samples exceeded the i16
    /// range (and were clamped), and the peak pre-clamp magnitude.
    clip_count: u64,
    raw_peak: f64,
    /// Interleaved stereo output frames awaiting the host.
    buffer: Vec<i16>,
}

impl Default for Apu {
    fn default() -> Self {
        Apu {
            regs: [0; 0x30],
            psg: Psg::new(),
            fifo: Default::default(),
            latch: [0; 2],
            dc_x: [0.0; 2],
            dc_y: [0.0; 2],
            lp_y: [0.0; 2],
            low_pass_on: true,
            mute_directsound: false,
            mute_psg: false,
            clip_count: 0,
            raw_peak: 0.0,
            buffer: Vec::new(),
        }
    }
}

impl Apu {
    pub fn new() -> Self {
        Self::default()
    }

    fn soundcnt_h(&self) -> u16 {
        u16::from_le_bytes([self.regs[0x22], self.regs[0x23]]) // 0x082 - 0x060
    }

    fn soundcnt_x(&self) -> u16 {
        u16::from_le_bytes([self.regs[0x24], self.regs[0x25]]) // 0x084 - 0x060
    }

    fn master_enabled(&self) -> bool {
        self.soundcnt_x() & X_MASTER_ENABLE != 0
    }

    /// Read a 16-bit sound register (`offset` relative to `0x4000000`).
    pub fn read16(&self, offset: u32) -> u16 {
        match offset {
            0x060..=0x081 | 0x090..=0x09F => self.psg.read16(offset),
            // NR52: our master-enable bit plus the PSG channel-on status bits.
            0x084 => (self.soundcnt_x() & X_MASTER_ENABLE) | self.psg.status_bits(),
            0x082..=0x08F => {
                let i = (offset - 0x060) as usize;
                u16::from_le_bytes([self.regs[i], self.regs[i + 1]])
            }
            _ => 0, // FIFOs are write-only
        }
    }

    /// Write a 16-bit sound register (masked). FIFO writes go through
    /// [`Apu::write_fifo`]; this handles the control/PSG block and wave RAM.
    pub fn write16(&mut self, offset: u32, value: u16, mask: u16) {
        match offset {
            0x060..=0x081 | 0x090..=0x09F => self.psg.write16(offset, value, mask),
            0x082..=0x08F => {
                let i = (offset - 0x060) as usize;
                let cur = u16::from_le_bytes([self.regs[i], self.regs[i + 1]]);
                let merged = (cur & !mask) | (value & mask);
                self.regs[i..i + 2].copy_from_slice(&merged.to_le_bytes());
                // A FIFO-reset bit clears the corresponding FIFO, then reads 0.
                if offset == 0x082 {
                    if merged & H_DSA_RESET != 0 {
                        self.fifo[0].clear();
                        self.regs[0x22] &= !((H_DSA_RESET & 0xFF) as u8);
                        self.regs[0x23] &= !((H_DSA_RESET >> 8) as u8);
                    }
                    if merged & H_DSB_RESET != 0 {
                        self.fifo[1].clear();
                        self.regs[0x22] &= !((H_DSB_RESET & 0xFF) as u8);
                        self.regs[0x23] &= !((H_DSB_RESET >> 8) as u8);
                    }
                }
            }
            _ => {}
        }
    }

    /// Push a 32-bit word into FIFO A (`index` 0, `0x0A0`) or B (`index` 1,
    /// `0x0A4`). Called for CPU and (mainly) DMA writes.
    pub fn write_fifo(&mut self, index: usize, word: u32) {
        self.fifo[index].push_word(word);
    }

    /// Clock the DirectSound channels whose selected timer (`timer_id`, 0 or 1)
    /// just overflowed: pop a sample into the latch. Returns which FIFOs (A, B)
    /// dropped to the refill threshold and need a DMA top-up.
    pub fn on_timer_overflow(&mut self, timer_id: usize) -> [bool; 2] {
        let h = self.soundcnt_h();
        let mut refill = [false; 2];
        let selects = [
            (h & H_DSA_TIMER != 0) as usize, // FIFO A timer
            (h & H_DSB_TIMER != 0) as usize, // FIFO B timer
        ];
        for ch in 0..2 {
            if selects[ch] == timer_id {
                self.latch[ch] = self.fifo[ch].pop();
                if self.fifo[ch].len <= FIFO_REFILL_THRESHOLD {
                    refill[ch] = true;
                }
            }
        }
        refill
    }

    /// Emit one interleaved stereo frame: advance the PSG one output sample and
    /// mix it with the DirectSound channels, then DC-block and clamp.
    pub fn generate_sample(&mut self) {
        let (mut pl, mut pr) = self.psg.step_sample(PSG_CYCLES_PER_SAMPLE);
        if self.mute_psg {
            (pl, pr) = (0, 0);
        }
        let (mut raw_l, mut raw_r) = (0i32, 0i32);
        if self.master_enabled() {
            let (dl, dr) = if self.mute_directsound { (0, 0) } else { self.directsound() };
            // SOUNDCNT_H bits 0-1 attenuate the PSG (25% / 50% / 100%).
            let shift = 2 - (self.soundcnt_h() & 3).min(2);
            raw_l = dl * DS_GAIN + (pl >> shift) * PSG_GAIN;
            raw_r = dr * DS_GAIN + (pr >> shift) * PSG_GAIN;
        }
        let l = self.dc_block(0, raw_l as f64);
        let r = self.dc_block(1, raw_r as f64);
        let l = self.low_pass(0, l);
        let r = self.low_pass(1, r);
        let magnitude = l.abs().max(r.abs());
        self.raw_peak = self.raw_peak.max(magnitude);
        if magnitude > i16::MAX as f64 {
            self.clip_count += 1;
        }
        self.buffer.push(l.clamp(i16::MIN as f64, i16::MAX as f64) as i16);
        self.buffer.push(r.clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }

    /// Clip metering since the last call: (samples clamped, peak pre-clamp
    /// magnitude). Resets the counters.
    pub fn clip_stats(&mut self) -> (u64, f64) {
        let stats = (self.clip_count, self.raw_peak);
        self.clip_count = 0;
        self.raw_peak = 0.0;
        stats
    }

    /// One-pole DC-blocking high-pass on output channel `ch`.
    fn dc_block(&mut self, ch: usize, x: f64) -> f64 {
        let y = x - self.dc_x[ch] + DC_BLOCK_R * self.dc_y[ch];
        self.dc_x[ch] = x;
        self.dc_y[ch] = y;
        y
    }

    /// One-pole output low-pass on channel `ch` (a no-op when disengaged, but the
    /// state keeps tracking so toggling it doesn't jump).
    fn low_pass(&mut self, ch: usize, x: f64) -> f64 {
        self.lp_y[ch] += LOW_PASS_ALPHA * (x - self.lp_y[ch]);
        if self.low_pass_on {
            self.lp_y[ch]
        } else {
            x
        }
    }

    /// Toggle the output low-pass filter; returns whether it is now engaged.
    pub fn toggle_low_pass(&mut self) -> bool {
        self.low_pass_on = !self.low_pass_on;
        self.low_pass_on
    }

    /// The DirectSound channels' contribution in natural units: each 8-bit sample
    /// scaled ×2 at full volume or ×1 at half, then panned. (`DS_GAIN` is applied
    /// by the caller.)
    fn directsound(&self) -> (i32, i32) {
        let h = self.soundcnt_h();
        let channel = |sample: i8, full: bool| -> i32 {
            let s = sample as i32;
            if full { s * 2 } else { s }
        };
        let a = channel(self.latch[0], h & H_DSA_VOLUME_FULL != 0);
        let b = channel(self.latch[1], h & H_DSB_VOLUME_FULL != 0);
        let mut left = 0i32;
        let mut right = 0i32;
        if h & H_DSA_ENABLE_LEFT != 0 {
            left += a;
        }
        if h & H_DSA_ENABLE_RIGHT != 0 {
            right += a;
        }
        if h & H_DSB_ENABLE_LEFT != 0 {
            left += b;
        }
        if h & H_DSB_ENABLE_RIGHT != 0 {
            right += b;
        }
        (left, right)
    }

    /// The DirectSound stereo pair at output scale, clamped — kept for tests.
    #[cfg(test)]
    fn mix(&self) -> (i16, i16) {
        if !self.master_enabled() {
            return (0, 0);
        }
        let (l, r) = self.directsound();
        (
            (l * DS_GAIN).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            (r * DS_GAIN).clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        )
    }

    /// Toggle the DirectSound mute (a debug aid for judging the mix); returns the
    /// new muted state.
    pub fn toggle_mute_directsound(&mut self) -> bool {
        self.mute_directsound = !self.mute_directsound;
        self.mute_directsound
    }

    /// Toggle the PSG mute; returns the new muted state.
    pub fn toggle_mute_psg(&mut self) -> bool {
        self.mute_psg = !self.mute_psg;
        self.mute_psg
    }

    /// Move the generated samples out for the host to play, leaving the buffer
    /// empty. Interleaved stereo (`[l, r, l, r, …]`).
    pub fn take_samples(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.buffer)
    }

    /// Number of interleaved samples buffered (2 per stereo frame).
    pub fn buffered_samples(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apu_with_dsa() -> Apu {
        let mut apu = Apu::new();
        // Master enable (SOUNDCNT_X bit 7).
        apu.write16(0x084, X_MASTER_ENABLE, 0xFFFF);
        // DSA: full volume, both speakers, Timer 0.
        apu.write16(0x082, H_DSA_VOLUME_FULL | H_DSA_ENABLE_LEFT | H_DSA_ENABLE_RIGHT, 0xFFFF);
        apu
    }

    #[test]
    fn fifo_word_push_and_timer_pop() {
        let mut apu = apu_with_dsa();
        apu.write_fifo(0, u32::from_le_bytes([10u8, 20, 30, 40]));
        // Timer 0 overflow pops the first sample into the latch.
        apu.on_timer_overflow(0);
        assert_eq!(apu.latch[0], 10);
        apu.on_timer_overflow(0);
        assert_eq!(apu.latch[0], 20);
    }

    #[test]
    fn wrong_timer_does_not_clock_channel() {
        let mut apu = apu_with_dsa(); // DSA on Timer 0
        apu.write_fifo(0, u32::from_le_bytes([7u8, 0, 0, 0]));
        apu.on_timer_overflow(1); // Timer 1: not DSA's timer
        assert_eq!(apu.latch[0], 0);
    }

    #[test]
    fn low_fifo_requests_refill() {
        let mut apu = apu_with_dsa();
        apu.write16(0x082, H_DSB_TIMER, H_DSB_TIMER); // FIFO B on Timer 1
        apu.write_fifo(0, 0); // 4 bytes -> below the 16-byte threshold
        let refill = apu.on_timer_overflow(0);
        assert!(refill[0]); // FIFO A (Timer 0) is low
        assert!(!refill[1]); // FIFO B is on Timer 1, not clocked here
    }

    #[test]
    fn mix_reflects_latched_sample_and_pan() {
        let mut apu = apu_with_dsa();
        apu.write_fifo(0, u32::from_le_bytes([100u8, 0, 0, 0]));
        apu.on_timer_overflow(0);
        let (l, r) = apu.mix();
        assert!(l > 0 && r > 0 && l == r);
    }

    #[test]
    fn master_disable_silences_output() {
        let mut apu = apu_with_dsa();
        apu.write16(0x084, 0, 0xFFFF); // clear master enable
        apu.write_fifo(0, u32::from_le_bytes([100u8, 0, 0, 0]));
        apu.on_timer_overflow(0);
        assert_eq!(apu.mix(), (0, 0));
    }

    #[test]
    fn generate_sample_appends_stereo_frame() {
        let mut apu = apu_with_dsa();
        apu.generate_sample();
        assert_eq!(apu.buffered_samples(), 2);
        assert_eq!(apu.take_samples().len(), 2);
        assert_eq!(apu.buffered_samples(), 0);
    }

    #[test]
    fn fifo_reset_bit_clears_and_reads_zero() {
        let mut apu = apu_with_dsa();
        apu.write_fifo(0, 0);
        apu.write16(0x082, H_DSA_RESET, H_DSA_RESET);
        assert_eq!(apu.soundcnt_h() & H_DSA_RESET, 0); // reset bit self-clears
        apu.on_timer_overflow(0);
        assert_eq!(apu.latch[0], 0); // FIFO was cleared
    }
}
