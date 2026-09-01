//! The DS sound engine: 16 channels mixed to a 32 kHz stereo stream.
//!
//! Each channel streams samples from memory (its `SOUNDxSAD` source address) at a
//! rate set by `SOUNDxTMR`, applies its own volume and panning, and the 16 channels
//! sum into a stereo output. A scheduler event samples the mix at a fixed output rate
//! and appends interleaved `i16` frames to a buffer the host drains — the same pull
//! model as the framebuffer, so pause/step/rewind stay deterministic.
//!
//! Phase A models the PCM8 and PCM16 formats; IMA-ADPCM (format 2) and the PSG/noise
//! channels (format 3, channels 8-15) decode as silence until later phases. Sound
//! capture (`SNDCAP`) is not modeled. Register layout follows GBATEK's DS sound.

use emu_core::Timestamp;

/// IMA-ADPCM index adjustment, selected by the low three bits of each 4-bit value.
const ADPCM_INDEX_TABLE: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];
/// IMA-ADPCM step table (89 entries), indexed by the running table index (0..=88).
const ADPCM_STEP_TABLE: [i32; 89] = [
    0x0007, 0x0008, 0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x000E, 0x0010, 0x0011, 0x0013, 0x0015,
    0x0017, 0x0019, 0x001C, 0x001F, 0x0022, 0x0025, 0x0029, 0x002D, 0x0032, 0x0037, 0x003C, 0x0042,
    0x0049, 0x0050, 0x0058, 0x0061, 0x006B, 0x0076, 0x0082, 0x008F, 0x009D, 0x00AD, 0x00BE, 0x00D1,
    0x00E6, 0x00FD, 0x0117, 0x0133, 0x0151, 0x0173, 0x0198, 0x01C1, 0x01EE, 0x0220, 0x0256, 0x0292,
    0x02D4, 0x031C, 0x036C, 0x03C3, 0x0424, 0x048E, 0x0502, 0x0583, 0x0610, 0x06AB, 0x0756, 0x0812,
    0x08E0, 0x09C3, 0x0ABD, 0x0BD0, 0x0CFF, 0x0E4C, 0x0FBA, 0x114C, 0x1307, 0x14EE, 0x1706, 0x1954,
    0x1BDC, 0x1EA5, 0x21B6, 0x2515, 0x28CA, 0x2CDF, 0x315B, 0x364B, 0x3BB9, 0x41B2, 0x4844, 0x4F7E,
    0x5771, 0x602F, 0x69CE, 0x7462, 0x7FFF,
];

/// The DS sound timer clock (the ARM7 clock), in Hz.
const SOUND_CLOCK: f64 = 33_513_982.0;
/// The mixer's output sample rate.
pub const SAMPLE_RATE: u32 = 32_768;
/// Master ticks (≈67 MHz) between emitted samples.
pub const CYCLES_PER_SAMPLE: Timestamp = 2046;

/// A channel's sample format (`SOUNDxCNT` bits 29-30).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Pcm8,
    Pcm16,
    Adpcm,
    Psg,
}

/// One of the 16 sound channels: its registers plus the playback cursor.
#[derive(Default)]
struct Channel {
    /// The channel's index 0..=15 (8-13 are PSG square, 14-15 are PSG noise).
    index: usize,
    /// `SOUNDxCNT`: volume/divider, panning, wave duty, repeat mode, format, start.
    cnt: u32,
    /// `SOUNDxSAD`: source byte address of the sample data.
    sad: u32,
    /// `SOUNDxTMR`: reload value; the sample rate is `SOUND_CLOCK / (0x10000 - tmr)`.
    tmr: u16,
    /// `SOUNDxPNT`: loop-start point, in 32-bit words from `sad`.
    pnt: u16,
    /// `SOUNDxLEN`: length after the loop point, in 32-bit words.
    len: u32,
    /// Whether the channel is currently producing sound.
    active: bool,
    /// Fractional sample cursor (units of source samples from `sad`).
    pos: f64,
    /// IMA-ADPCM decoder state (only used for format 2). The header is read lazily on
    /// the first sample; the decoder runs sequentially, capturing its predictor/index
    /// at the loop point and restoring them on each loop (as the hardware does).
    adpcm_started: bool,
    adpcm_pcm: i32,
    adpcm_index: i32,
    /// Index of the last nibble decoded into `adpcm_pcm`.
    adpcm_decoded: u32,
    adpcm_loop_pcm: i32,
    adpcm_loop_index: i32,
    adpcm_loop_captured: bool,
    /// PSG-noise 15-bit LFSR state (only channels 14-15).
    noise_started: bool,
    noise_lfsr: u16,
    noise_stepped: u32,
}

impl Channel {
    fn format(&self) -> Format {
        match (self.cnt >> 29) & 3 {
            0 => Format::Pcm8,
            1 => Format::Pcm16,
            2 => Format::Adpcm,
            _ => Format::Psg,
        }
    }

    /// Whether the repeat mode (bits 27-28) is "loop infinite".
    fn loops(&self) -> bool {
        (self.cnt >> 27) & 3 == 1
    }

    /// Total playable samples and the loop-start sample. PCM counts 4 (PCM8) or 2
    /// (PCM16) samples per word; ADPCM counts 8 nibbles per word after the one-word
    /// header (so its word counts drop the header).
    fn total_samples(&self) -> u32 {
        let words = self.pnt as u32 + self.len;
        match self.format() {
            Format::Pcm8 => words * 4,
            Format::Pcm16 => words * 2,
            Format::Adpcm => words.saturating_sub(1) * 8,
            Format::Psg => 0,
        }
    }
    fn loop_start_samples(&self) -> u32 {
        let words = self.pnt as u32;
        match self.format() {
            Format::Pcm8 => words * 4,
            Format::Pcm16 => words * 2,
            Format::Adpcm => words.saturating_sub(1) * 8,
            Format::Psg => 0,
        }
    }

    /// Source samples advanced per output sample: the channel rate over the mix rate.
    fn step(&self) -> f64 {
        let period = (0x1_0000 - self.tmr as u32).max(1) as f64;
        SOUND_CLOCK / (period * SAMPLE_RATE as f64)
    }

    /// Start (or restart) playback: cursor to the top of the sample data. The ADPCM
    /// header is read lazily on the first sample (memory isn't reachable here).
    fn start(&mut self) {
        self.active = true;
        self.pos = 0.0;
        self.adpcm_started = false;
        self.adpcm_decoded = 0;
        self.adpcm_loop_captured = false;
        self.noise_started = false;
    }

    /// Advance the PSG-noise 15-bit LFSR to the current sample position; the output is
    /// its inverted low bit at full scale.
    fn noise_sample(&mut self) -> i32 {
        if !self.noise_started {
            self.noise_lfsr = 0x7FFF;
            self.noise_stepped = 0;
            self.noise_started = true;
        }
        let target = self.pos as u32;
        while self.noise_stepped < target {
            self.noise_stepped += 1;
            let carry = (self.noise_lfsr ^ (self.noise_lfsr >> 1)) & 1;
            self.noise_lfsr = (self.noise_lfsr >> 1) | (carry << 14);
        }
        if self.noise_lfsr & 1 != 0 {
            -0x7FFF
        } else {
            0x7FFF
        }
    }

    /// A PSG square-wave sample: `SOUNDxCNT` wave duty (bits 24-26) sets how many of
    /// the eight phase steps are high (`duty+1` of 8 → 12.5%..100%).
    fn square_sample(&self) -> i32 {
        let duty = (self.cnt >> 24) & 7;
        if (self.pos as u32) & 7 <= duty {
            0x7FFF
        } else {
            -0x7FFF
        }
    }

    /// Decode one IMA-ADPCM nibble (index `n` from the first data nibble) into the
    /// running predictor `adpcm_pcm`, and update the step `adpcm_index`.
    fn decode_nibble(&mut self, n: u32, read: &impl Fn(u32) -> u8) {
        // The 4-byte header precedes the nibble stream; two nibbles per byte.
        let byte = read(self.sad + 4 + n / 2);
        let data = if n & 1 == 0 { byte & 0xF } else { byte >> 4 } as i32;
        let step = ADPCM_STEP_TABLE[self.adpcm_index as usize];
        let mut diff = step >> 3;
        if data & 1 != 0 {
            diff += step >> 2;
        }
        if data & 2 != 0 {
            diff += step >> 1;
        }
        if data & 4 != 0 {
            diff += step;
        }
        if data & 8 != 0 {
            self.adpcm_pcm = (self.adpcm_pcm - diff).max(-0x7FFF);
        } else {
            self.adpcm_pcm = (self.adpcm_pcm + diff).min(0x7FFF);
        }
        self.adpcm_index = (self.adpcm_index + ADPCM_INDEX_TABLE[(data & 7) as usize]).clamp(0, 88);
    }

    /// Capture the decoder's predictor/index once, when it reaches the loop point, so
    /// a loop can restore exactly that state (as the hardware does).
    fn maybe_capture_loop(&mut self) {
        if self.loops() && !self.adpcm_loop_captured && self.adpcm_decoded == self.loop_start_samples()
        {
            self.adpcm_loop_pcm = self.adpcm_pcm;
            self.adpcm_loop_index = self.adpcm_index;
            self.adpcm_loop_captured = true;
        }
    }

    /// Decode ADPCM forward until `adpcm_pcm` reflects the sample at `pos`.
    fn decode_forward(&mut self, read: &impl Fn(u32) -> u8) {
        if !self.adpcm_started {
            let h = read(self.sad) as u32
                | (read(self.sad + 1) as u32) << 8
                | (read(self.sad + 2) as u32) << 16
                | (read(self.sad + 3) as u32) << 24;
            self.adpcm_pcm = (h & 0xFFFF) as i16 as i32;
            self.adpcm_index = ((h >> 16) & 0x7F).min(88) as i32;
            self.adpcm_started = true;
            self.decode_nibble(0, read);
            self.adpcm_decoded = 0;
            self.maybe_capture_loop();
        }
        let target = self.pos as u32;
        while self.adpcm_decoded < target {
            self.adpcm_decoded += 1;
            self.decode_nibble(self.adpcm_decoded, read);
            self.maybe_capture_loop();
        }
    }

    /// The current mono sample (i16 range): a stateless PCM read, or the running
    /// ADPCM predictor decoded up to the cursor.
    fn current_sample(&mut self, read: &impl Fn(u32) -> u8) -> i32 {
        match self.format() {
            Format::Pcm8 => {
                let i = self.pos as u32;
                (read(self.sad + i) as i8 as i32) << 8
            }
            Format::Pcm16 => {
                let i = self.pos as u32;
                let lo = read(self.sad + i * 2) as u16;
                let hi = read(self.sad + i * 2 + 1) as u16;
                (lo | (hi << 8)) as i16 as i32
            }
            Format::Adpcm => {
                self.decode_forward(read);
                self.adpcm_pcm
            }
            // PSG is only valid on channels 8-15: 8-13 square, 14-15 noise.
            Format::Psg if self.index >= 14 => self.noise_sample(),
            Format::Psg if self.index >= 8 => self.square_sample(),
            Format::Psg => 0,
        }
    }

    /// Advance the cursor one output sample, wrapping at the loop point or stopping.
    /// On an ADPCM loop the captured predictor/index are restored.
    fn advance(&mut self) {
        self.pos += self.step();
        let total = self.total_samples() as f64;
        if total <= 0.0 || self.pos < total {
            return;
        }
        if self.loops() {
            let ls = self.loop_start_samples() as f64;
            let ll = (total - ls).max(1.0);
            self.pos = ls + (self.pos - ls) % ll;
            if self.format() == Format::Adpcm {
                self.adpcm_pcm = self.adpcm_loop_pcm;
                self.adpcm_index = self.adpcm_loop_index;
                self.adpcm_decoded = self.loop_start_samples();
            }
        } else {
            self.active = false;
        }
    }

    /// This channel's `(left, right)` contribution for one output sample: fetch the
    /// current sample, apply volume and panning, then advance the cursor.
    fn next(&mut self, read: &impl Fn(u32) -> u8) -> (i32, i32) {
        let sample = self.current_sample(read);
        // Volume: a 0..127 multiplier and a 0/1/2/4 → ÷1/÷2/÷4/÷16 divider.
        let mul = (self.cnt & 0x7F) as i32;
        let div_shift = match (self.cnt >> 8) & 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let v = ((sample * mul) >> 7) >> div_shift;
        // Panning: 0 = full left, 127 = full right, 64 = centered.
        let pan = ((self.cnt >> 16) & 0x7F) as i32;
        self.advance();
        ((v * (127 - pan)) >> 7, (v * pan) >> 7)
    }
}

/// The 16-channel sound engine and the master control registers.
pub struct Sound {
    channels: [Channel; 16],
    /// `SOUNDCNT` (`0x4000500`): master volume (bits 0-6) and master enable (bit 15).
    soundcnt: u16,
    /// `SOUNDBIAS` (`0x4000504`): stored for readback; not applied to the output.
    soundbias: u16,
    /// Interleaved stereo output the host drains via [`Self::take_samples`].
    buffer: Vec<i16>,
}

impl Default for Sound {
    fn default() -> Self {
        Sound::new()
    }
}

impl Sound {
    pub fn new() -> Self {
        Sound {
            channels: std::array::from_fn(|i| Channel {
                index: i,
                ..Channel::default()
            }),
            soundcnt: 0,
            soundbias: 0x200,
            buffer: Vec::new(),
        }
    }

    /// Read a sound register (`offset` relative to `0x0400_0000`). Only `SOUNDxCNT`,
    /// `SOUNDCNT`, and `SOUNDBIAS` are meaningfully readable.
    pub fn read(&self, offset: u32, bytes: u32) -> u32 {
        let full = if (0x400..0x500).contains(&offset) {
            let ch = ((offset - 0x400) / 0x10) as usize;
            match (offset - 0x400) % 0x10 {
                0x0..=0x3 => self.channels[ch].cnt,
                _ => 0, // SAD/TMR/PNT/LEN are write-only
            }
        } else if (0x500..0x502).contains(&offset) {
            self.soundcnt as u32
        } else if (0x504..0x506).contains(&offset) {
            self.soundbias as u32
        } else {
            0
        };
        let shift = (offset & 3) * 8;
        let v = full >> shift;
        match bytes {
            4 => full,
            2 => v & 0xFFFF,
            _ => v & 0xFF,
        }
    }

    /// Write a sound register (`offset` relative to `0x0400_0000`).
    pub fn write(&mut self, offset: u32, value: u32, bytes: u32) {
        if (0x400..0x500).contains(&offset) {
            let ch = ((offset - 0x400) / 0x10) as usize;
            let reg = (offset - 0x400) % 0x10;
            let c = &mut self.channels[ch];
            match reg {
                0x0..=0x3 => {
                    let was_on = c.cnt & (1 << 31) != 0;
                    c.cnt = splice(c.cnt, reg, value, bytes);
                    let on = c.cnt & (1 << 31) != 0;
                    if on && !was_on {
                        c.start();
                    } else if !on {
                        c.active = false;
                    }
                }
                0x4..=0x7 => c.sad = splice(c.sad, reg - 0x4, value, bytes),
                0x8 | 0x9 => c.tmr = splice(c.tmr as u32, reg - 0x8, value, bytes) as u16,
                0xA | 0xB => c.pnt = splice(c.pnt as u32, reg - 0xA, value, bytes) as u16,
                _ => c.len = splice(c.len, reg - 0xC, value, bytes) & 0x3F_FFFF,
            }
        } else if (0x500..0x502).contains(&offset) {
            self.soundcnt = splice(self.soundcnt as u32, offset - 0x500, value, bytes) as u16;
        } else if (0x504..0x506).contains(&offset) {
            self.soundbias = splice(self.soundbias as u32, offset - 0x504, value, bytes) as u16 & 0x3FF;
        }
    }

    /// Emit one output sample: sum every active channel (under the master enable and
    /// volume) and append the interleaved stereo frame. `read` fetches a source byte.
    pub fn generate_sample(&mut self, read: impl Fn(u32) -> u8) {
        let (mut l, mut r) = (0i32, 0i32);
        if self.soundcnt & (1 << 15) != 0 {
            for ch in &mut self.channels {
                if !ch.active {
                    continue;
                }
                let (cl, cr) = ch.next(&read);
                l += cl;
                r += cr;
            }
            // Master volume (bits 0-6, 0..127).
            let mvol = (self.soundcnt & 0x7F) as i32;
            l = (l * mvol) >> 7;
            r = (r * mvol) >> 7;
        }
        self.buffer.push(l.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
        self.buffer.push(r.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    }

    /// Drain the accumulated interleaved stereo samples.
    pub fn take_samples(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.buffer)
    }

    /// Debug: a channel's `(sad, tmr, len, pos)` for tracing playback.
    pub fn channel_debug(&self, ch: usize) -> (u32, u16, u32, u32) {
        let c = &self.channels[ch];
        (c.sad, c.tmr, c.len, c.pos as u32)
    }

    /// Debug snapshot: (master enabled, active-channel count, a per-channel active
    /// bitmask). For validating that a game has set up the sound engine.
    pub fn status(&self) -> (bool, usize, u16) {
        let mut mask = 0u16;
        for (i, ch) in self.channels.iter().enumerate() {
            if ch.active {
                mask |= 1 << i;
            }
        }
        (self.soundcnt & (1 << 15) != 0, mask.count_ones() as usize, mask)
    }
}

/// Overwrite `bytes` bytes of `reg` at byte offset `off` with `value`.
fn splice(reg: u32, off: u32, value: u32, bytes: u32) -> u32 {
    let shift = off * 8;
    match bytes {
        4 => value,
        2 => (reg & !(0xFFFF << shift)) | ((value & 0xFFFF) << shift),
        _ => (reg & !(0xFF << shift)) | ((value & 0xFF) << shift),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A started PCM8 channel plays its samples, applies volume, and one-shot stops.
    #[test]
    fn pcm8_one_shot_plays_then_stops() {
        let mut s = Sound::new();
        // Sample data: a constant +64 (i8) at address 0x0200_0000.
        let data = |addr: u32| -> u8 {
            if (0x0200_0000..0x0200_0004).contains(&addr) {
                64
            } else {
                0
            }
        };
        s.write(0x500, 1 << 15 | 127, 2); // SOUNDCNT: master enable + full volume
        s.write(0x404, 0x0200_0000, 4); // ch0 SAD
        s.write(0x408, 0xFFFF, 2); // ch0 TMR: fastest (period 1) so it advances quickly
        s.write(0x40C, 1, 4); // ch0 LEN = 1 word = 4 PCM8 samples
        // Start: one-shot (repeat=2), PCM8 (format=0), full channel volume, center pan.
        s.write(0x400, (1 << 31) | (2 << 27) | (64 << 16) | 127, 4);
        s.generate_sample(data); // first sample: +64<<8 scaled by volumes
        assert!(s.buffer[0] != 0, "channel should produce non-zero output");
        // Drive past the 4 samples; the one-shot channel must go silent.
        for _ in 0..8 {
            s.generate_sample(data);
        }
        assert!(!s.channels[0].active, "one-shot channel stops after its length");
    }

    /// An IMA-ADPCM channel decodes its nibble stream: a run of +step nibbles drives
    /// the predictor upward from the header's initial value.
    #[test]
    fn adpcm_decodes_a_rising_predictor() {
        let mut s = Sound::new();
        // Header (pcm=0, index=0) at 0x0200_0000; then nibbles all 4 (positive step).
        let data = |addr: u32| -> u8 {
            if (0x0200_0000..0x0200_0004).contains(&addr) {
                0 // header: initial pcm 0, index 0
            } else {
                0x44 // two nibbles of 4: add a step each
            }
        };
        s.write(0x500, (1 << 15) | 127, 2); // master enable + full volume
        s.write(0x404, 0x0200_0000, 4); // SAD
        s.write(0x408, 0xFC00, 2); // TMR → ~1 nibble per output sample
        s.write(0x40C, 8, 4); // LEN = 8 words
        // Start: one-shot, ADPCM (format 2), full channel volume, center pan.
        s.write(0x400, (1 << 31) | (2 << 27) | (2 << 29) | (64 << 16) | 127, 4);
        let mut first = 0;
        let mut last = 0;
        for k in 0..8 {
            s.generate_sample(data);
            let v = s.buffer[s.buffer.len() - 2] as i32; // left channel
            if k == 0 {
                first = v;
            }
            last = v;
        }
        assert!(last > first, "the ADPCM predictor should rise ({first} -> {last})");
        assert!(last > 0, "a run of positive nibbles yields positive output");
    }

    /// A PSG square channel (ch 8) oscillates: over one duty cycle it takes both a
    /// positive and a negative value.
    #[test]
    fn psg_square_oscillates() {
        let mut s = Sound::new();
        s.write(0x500, (1 << 15) | 127, 2); // master enable + full volume
        // Channel 8 registers start at 0x400 + 8*0x10 = 0x480.
        s.write(0x488, 0xF000, 2); // TMR: a few phase steps per output sample
        // Start ch8: PSG (format 3), 50% duty (3), full vol, center pan, loop.
        s.write(0x480, (1 << 31) | (1 << 27) | (3 << 29) | (3 << 24) | (64 << 16) | 127, 4);
        let mut saw_pos = false;
        let mut saw_neg = false;
        for _ in 0..64 {
            s.generate_sample(|_| 0);
            let v = s.buffer[s.buffer.len() - 2] as i32;
            saw_pos |= v > 0;
            saw_neg |= v < 0;
        }
        assert!(saw_pos && saw_neg, "a square wave should swing both ways");
    }

    /// A PSG noise channel (ch 15) produces a varying, non-constant signal.
    #[test]
    fn psg_noise_varies() {
        let mut s = Sound::new();
        s.write(0x500, (1 << 15) | 127, 2);
        // Channel 15 registers start at 0x400 + 15*0x10 = 0x4F0.
        s.write(0x4F8, 0xFC00, 2); // TMR
        s.write(0x4F0, (1 << 31) | (1 << 27) | (3 << 29) | (64 << 16) | 127, 4);
        let mut values = std::collections::HashSet::new();
        for _ in 0..64 {
            s.generate_sample(|_| 0);
            values.insert(s.buffer[s.buffer.len() - 2]);
        }
        assert!(values.len() > 1, "noise should not be a constant value");
    }

    #[test]
    fn master_disable_silences_output() {
        let mut s = Sound::new();
        s.write(0x404, 0x0200_0000, 4);
        s.write(0x40C, 1, 4);
        s.write(0x400, (1 << 31) | 127, 4);
        s.generate_sample(|_| 64); // master disabled (SOUNDCNT bit 15 clear)
        assert_eq!(s.buffer[0], 0);
        assert_eq!(s.buffer[1], 0);
    }
}
