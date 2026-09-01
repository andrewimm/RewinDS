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

    /// Samples per 32-bit source word for the current format.
    fn samples_per_word(&self) -> u32 {
        match self.format() {
            Format::Pcm8 => 4,
            Format::Pcm16 => 2,
            // ADPCM packs 8 nibbles/word after a 4-byte header; PSG has no data.
            _ => 1,
        }
    }

    /// Total playable samples (`(pnt + len)` words) and the loop-start sample.
    fn total_samples(&self) -> u32 {
        (self.pnt as u32 + self.len) * self.samples_per_word()
    }
    fn loop_start_samples(&self) -> u32 {
        self.pnt as u32 * self.samples_per_word()
    }

    /// Source samples advanced per output sample: the channel rate over the mix rate.
    fn step(&self) -> f64 {
        let period = (0x1_0000 - self.tmr as u32).max(1) as f64;
        SOUND_CLOCK / (period * SAMPLE_RATE as f64)
    }

    /// Start (or restart) playback: cursor to the top of the sample data.
    fn start(&mut self) {
        self.active = true;
        self.pos = 0.0;
    }

    /// The current mono sample, scaled to the i16 range, via `read` (a raw byte fetch).
    fn sample(&self, read: &impl Fn(u32) -> u8) -> i32 {
        let i = self.pos as u32;
        match self.format() {
            Format::Pcm8 => (read(self.sad + i) as i8 as i32) << 8,
            Format::Pcm16 => {
                let lo = read(self.sad + i * 2) as u16;
                let hi = read(self.sad + i * 2 + 1) as u16;
                (lo | (hi << 8)) as i16 as i32
            }
            // ADPCM / PSG: later phases.
            _ => 0,
        }
    }

    /// Advance the cursor one output sample, wrapping at the loop point or stopping.
    fn advance(&mut self) {
        self.pos += self.step();
        let total = self.total_samples() as f64;
        if total <= 0.0 || self.pos < total {
            return;
        }
        // Repeat mode (bits 27-28): 1 = loop, else one-shot/manual → stop.
        if (self.cnt >> 27) & 3 == 1 {
            let loop_start = self.loop_start_samples() as f64;
            let loop_len = (total - loop_start).max(1.0);
            self.pos = loop_start + (self.pos - loop_start) % loop_len;
        } else {
            self.active = false;
        }
    }

    /// `(left, right)` contribution, after the channel's own volume and panning.
    fn output(&self, read: &impl Fn(u32) -> u8) -> (i32, i32) {
        let sample = self.sample(read);
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
            channels: std::array::from_fn(|_| Channel::default()),
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
                let (cl, cr) = ch.output(&read);
                l += cl;
                r += cr;
                ch.advance();
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
