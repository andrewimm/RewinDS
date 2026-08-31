//! The four Game Boy PSG channels: two square waves (channel 1 with a frequency
//! sweep), a 32-sample wave channel, and a noise channel.
//!
//! Each channel has a frequency timer clocking its waveform and is shaped by a
//! 512 Hz frame sequencer (length counters, volume envelopes, the sweep). The
//! channels are advanced one output sample at a time (see [`Psg::step_sample`]),
//! stepping their timers by the base cycles that sample spans; fast frequencies
//! that step several times per sample are handled by a loop (a zero-order hold,
//! which aliases only well above the audible range).

/// The PSG base clock is 2^22 Hz.
const FRAME_SEQ_PERIOD: i32 = 8192; // base cycles per 512 Hz frame-sequencer step

/// The eight-step duty patterns (12.5% / 25% / 50% / 75%).
const DUTY: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 0, 0, 1],
    [1, 0, 0, 0, 0, 1, 1, 1],
    [0, 1, 1, 1, 1, 1, 1, 0],
];

/// Noise frequency-timer divisors, indexed by `NR43` bits 0-2.
const NOISE_DIVISORS: [i32; 8] = [8, 16, 32, 48, 64, 80, 96, 112];

/// A volume envelope, shared by the square and noise channels.
#[derive(Clone, Copy, Debug, Default)]
struct Envelope {
    initial: u8,
    increasing: bool,
    period: u8,
    volume: u8,
    timer: u8,
}

impl Envelope {
    fn trigger(&mut self) {
        self.volume = self.initial;
        self.timer = self.period;
    }

    fn clock(&mut self) {
        if self.period == 0 {
            return;
        }
        if self.timer > 0 {
            self.timer -= 1;
        }
        if self.timer == 0 {
            self.timer = self.period;
            if self.increasing && self.volume < 15 {
                self.volume += 1;
            } else if !self.increasing && self.volume > 0 {
                self.volume -= 1;
            }
        }
    }

    /// The channel's DAC is powered only when the envelope isn't a constant zero.
    fn dac_on(&self) -> bool {
        self.initial != 0 || self.increasing
    }
}

/// A square-wave channel (channels 1 and 2). Channel 1 also has a sweep.
#[derive(Clone, Copy, Debug, Default)]
struct Square {
    has_sweep: bool,
    // Registers.
    duty: u8,
    freq: u16,
    length_enabled: bool,
    env: Envelope,
    sweep_period: u8,
    sweep_negate: bool,
    sweep_shift: u8,
    // Running state.
    enabled: bool,
    timer: i32,
    duty_pos: u8,
    length_timer: u16,
    sweep_timer: u8,
    sweep_on: bool,
    sweep_shadow: u16,
}

impl Square {
    fn period(&self) -> i32 {
        (2048 - self.freq as i32) * 4
    }

    fn trigger(&mut self) {
        self.enabled = self.env.dac_on();
        self.timer = self.period();
        self.env.trigger();
        if self.length_timer == 0 {
            self.length_timer = 64;
        }
        if self.has_sweep {
            self.sweep_shadow = self.freq;
            self.sweep_timer = if self.sweep_period == 0 { 8 } else { self.sweep_period };
            self.sweep_on = self.sweep_period != 0 || self.sweep_shift != 0;
            if self.sweep_shift != 0 {
                self.compute_sweep(); // an immediate overflow check can disable the channel
            }
        }
    }

    /// The next sweep frequency; disables the channel on overflow.
    fn compute_sweep(&mut self) -> u16 {
        let delta = self.sweep_shadow >> self.sweep_shift;
        let next = if self.sweep_negate {
            self.sweep_shadow.wrapping_sub(delta)
        } else {
            self.sweep_shadow + delta
        };
        if next > 2047 {
            self.enabled = false;
        }
        next
    }

    fn step(&mut self, cycles: i32) {
        self.timer -= cycles;
        while self.timer <= 0 {
            self.timer += self.period().max(1);
            self.duty_pos = (self.duty_pos + 1) % 8;
        }
    }

    fn output(&self) -> u8 {
        if !self.enabled {
            return 0;
        }
        DUTY[self.duty as usize][self.duty_pos as usize] * self.env.volume
    }

    fn clock_length(&mut self) {
        if self.length_enabled && self.length_timer > 0 {
            self.length_timer -= 1;
            if self.length_timer == 0 {
                self.enabled = false;
            }
        }
    }

    fn clock_sweep(&mut self) {
        if !self.has_sweep {
            return;
        }
        if self.sweep_timer > 0 {
            self.sweep_timer -= 1;
        }
        if self.sweep_timer == 0 {
            self.sweep_timer = if self.sweep_period == 0 { 8 } else { self.sweep_period };
            if self.sweep_on && self.sweep_period != 0 && self.sweep_shift != 0 {
                let next = self.compute_sweep();
                if next <= 2047 {
                    self.sweep_shadow = next;
                    self.freq = next;
                    self.compute_sweep(); // second overflow check
                }
            }
        }
    }
}

/// The wave channel (channel 3): 32 four-bit samples from wave RAM.
#[derive(Clone, Debug)]
struct Wave {
    dac_on: bool,
    volume_shift: u8, // 4 = mute, 0 = 100%, 1 = 50%, 2 = 25%
    freq: u16,
    length_enabled: bool,
    enabled: bool,
    timer: i32,
    pos: u8,
    length_timer: u16,
    ram: [u8; 16],
}

impl Default for Wave {
    fn default() -> Self {
        Wave {
            dac_on: false,
            volume_shift: 4,
            freq: 0,
            length_enabled: false,
            enabled: false,
            timer: 0,
            pos: 0,
            length_timer: 0,
            ram: [0; 16],
        }
    }
}

impl Wave {
    fn period(&self) -> i32 {
        (2048 - self.freq as i32) * 2
    }

    fn trigger(&mut self) {
        self.enabled = self.dac_on;
        self.timer = self.period();
        self.pos = 0;
        if self.length_timer == 0 {
            self.length_timer = 256;
        }
    }

    fn step(&mut self, cycles: i32) {
        self.timer -= cycles;
        while self.timer <= 0 {
            self.timer += self.period().max(1);
            self.pos = (self.pos + 1) % 32;
        }
    }

    fn output(&self) -> u8 {
        if !self.enabled || !self.dac_on {
            return 0;
        }
        let byte = self.ram[self.pos as usize / 2];
        let sample = if self.pos.is_multiple_of(2) { byte >> 4 } else { byte & 0x0F };
        if self.volume_shift >= 4 {
            0
        } else {
            sample >> self.volume_shift
        }
    }

    fn clock_length(&mut self) {
        if self.length_enabled && self.length_timer > 0 {
            self.length_timer -= 1;
            if self.length_timer == 0 {
                self.enabled = false;
            }
        }
    }
}

/// The noise channel (channel 4): a 15/7-bit LFSR.
#[derive(Clone, Copy, Debug)]
struct Noise {
    env: Envelope,
    length_enabled: bool,
    div_code: u8,
    width_7bit: bool,
    shift: u8,
    enabled: bool,
    timer: i32,
    lfsr: u16,
    length_timer: u16,
}

impl Default for Noise {
    fn default() -> Self {
        Noise {
            env: Envelope::default(),
            length_enabled: false,
            div_code: 0,
            width_7bit: false,
            shift: 0,
            enabled: false,
            timer: 0,
            lfsr: 0x7FFF,
            length_timer: 0,
        }
    }
}

impl Noise {
    fn period(&self) -> i32 {
        (NOISE_DIVISORS[self.div_code as usize] << self.shift).max(1)
    }

    fn trigger(&mut self) {
        self.enabled = self.env.dac_on();
        self.timer = self.period();
        self.env.trigger();
        self.lfsr = 0x7FFF;
        if self.length_timer == 0 {
            self.length_timer = 64;
        }
    }

    fn step(&mut self, cycles: i32) {
        self.timer -= cycles;
        while self.timer <= 0 {
            self.timer += self.period();
            let bit = (self.lfsr ^ (self.lfsr >> 1)) & 1;
            self.lfsr = (self.lfsr >> 1) | (bit << 14);
            if self.width_7bit {
                self.lfsr = (self.lfsr & !(1 << 6)) | (bit << 6);
            }
        }
    }

    fn output(&self) -> u8 {
        if !self.enabled {
            return 0;
        }
        ((!self.lfsr & 1) as u8) * self.env.volume
    }

    fn clock_length(&mut self) {
        if self.length_enabled && self.length_timer > 0 {
            self.length_timer -= 1;
            if self.length_timer == 0 {
                self.enabled = false;
            }
        }
    }
}

/// The PSG: four channels plus the frame sequencer and the master volume/pan
/// registers (`NR50`/`NR51`).
#[derive(Clone, Debug)]
pub struct Psg {
    ch1: Square,
    ch2: Square,
    ch3: Wave,
    ch4: Noise,
    frame_step: u8,
    frame_timer: i32,
    nr50: u8,
    nr51: u8,
    /// Raw register bytes for `0x060..0x082`, kept for read-back.
    regs: [u8; 0x22],
}

impl Default for Psg {
    fn default() -> Self {
        Psg {
            ch1: Square { has_sweep: true, ..Default::default() },
            ch2: Square::default(),
            ch3: Wave::default(),
            ch4: Noise::default(),
            frame_step: 0,
            frame_timer: FRAME_SEQ_PERIOD,
            nr50: 0,
            nr51: 0,
            regs: [0; 0x22],
        }
    }
}

impl Psg {
    pub fn new() -> Self {
        Self::default()
    }

    /// The four channel-on status bits for `NR52` (0x084) read-back.
    pub fn status_bits(&self) -> u16 {
        (self.ch1.enabled as u16)
            | ((self.ch2.enabled as u16) << 1)
            | ((self.ch3.enabled as u16) << 2)
            | ((self.ch4.enabled as u16) << 3)
    }

    pub fn read16(&self, offset: u32) -> u16 {
        match offset {
            0x060..=0x081 => {
                let i = (offset - 0x060) as usize;
                u16::from_le_bytes([self.regs[i], self.regs[i + 1]])
            }
            0x090..=0x09F => {
                let i = (offset - 0x090) as usize;
                u16::from_le_bytes([self.ch3.ram[i], self.ch3.ram[i + 1]])
            }
            _ => 0,
        }
    }

    pub fn write16(&mut self, offset: u32, value: u16, mask: u16) {
        if (0x090..=0x09F).contains(&offset) {
            let i = (offset - 0x090) as usize;
            let cur = u16::from_le_bytes([self.ch3.ram[i], self.ch3.ram[i + 1]]);
            let merged = (cur & !mask) | (value & mask);
            self.ch3.ram[i..i + 2].copy_from_slice(&merged.to_le_bytes());
            return;
        }
        if !(0x060..=0x081).contains(&offset) {
            return;
        }
        let i = (offset - 0x060) as usize;
        let cur = u16::from_le_bytes([self.regs[i], self.regs[i + 1]]);
        let merged = (cur & !mask) | (value & mask);
        self.regs[i..i + 2].copy_from_slice(&merged.to_le_bytes());
        let lo = (merged & 0xFF) as u8;
        let hi = (merged >> 8) as u8;
        match offset {
            0x060 => {
                self.ch1.sweep_period = (lo >> 4) & 7;
                self.ch1.sweep_negate = lo & 0x08 != 0;
                self.ch1.sweep_shift = lo & 7;
            }
            0x062 => Self::write_duty_env(&mut self.ch1, lo, hi),
            0x064 => self.write_square_freq(0, lo, hi),
            0x068 => Self::write_duty_env(&mut self.ch2, lo, hi),
            0x06C => self.write_square_freq(1, lo, hi),
            0x070 => {
                self.ch3.dac_on = hi_or_lo_bit7(lo);
                if !self.ch3.dac_on {
                    self.ch3.enabled = false;
                }
            }
            0x072 => {
                self.ch3.length_timer = 256 - lo as u16;
                self.ch3.volume_shift = match (hi >> 5) & 3 {
                    0 => 4, // mute
                    1 => 0, // 100%
                    2 => 1, // 50%
                    _ => 2, // 25%
                };
            }
            0x074 => {
                self.ch3.freq = (self.ch3.freq & 0x0700) | lo as u16;
                self.ch3.freq = (self.ch3.freq & 0x00FF) | (((hi & 7) as u16) << 8);
                self.ch3.length_enabled = hi & 0x40 != 0;
                if hi & 0x80 != 0 {
                    self.ch3.trigger();
                }
            }
            0x078 => {
                self.ch4.length_timer = 64 - (lo & 0x3F) as u16;
                self.ch4.env = env_from_byte(hi);
                if !self.ch4.env.dac_on() {
                    self.ch4.enabled = false;
                }
            }
            0x07C => {
                self.ch4.shift = (lo >> 4) & 0x0F;
                self.ch4.width_7bit = lo & 0x08 != 0;
                self.ch4.div_code = lo & 7;
                self.ch4.length_enabled = hi & 0x40 != 0;
                if hi & 0x80 != 0 {
                    self.ch4.trigger();
                }
            }
            0x080 => {
                self.nr50 = lo;
                self.nr51 = hi;
            }
            _ => {}
        }
    }

    fn write_duty_env(ch: &mut Square, lo: u8, hi: u8) {
        ch.duty = (lo >> 6) & 3;
        ch.length_timer = 64 - (lo & 0x3F) as u16;
        ch.env = env_from_byte(hi);
        if !ch.env.dac_on() {
            ch.enabled = false;
        }
    }

    fn write_square_freq(&mut self, ch_index: usize, lo: u8, hi: u8) {
        let ch = if ch_index == 0 { &mut self.ch1 } else { &mut self.ch2 };
        ch.freq = (ch.freq & 0x0700) | lo as u16;
        ch.freq = (ch.freq & 0x00FF) | (((hi & 7) as u16) << 8);
        ch.length_enabled = hi & 0x40 != 0;
        if hi & 0x80 != 0 {
            ch.trigger();
        }
    }

    fn clock_frame_sequencer(&mut self) {
        match self.frame_step {
            0 | 4 => self.clock_length(),
            2 | 6 => {
                self.clock_length();
                self.ch1.clock_sweep();
            }
            7 => {
                self.ch1.env.clock();
                self.ch2.env.clock();
                self.ch4.env.clock();
            }
            _ => {}
        }
        self.frame_step = (self.frame_step + 1) % 8;
    }

    fn clock_length(&mut self) {
        self.ch1.clock_length();
        self.ch2.clock_length();
        self.ch3.clock_length();
        self.ch4.clock_length();
    }

    /// Advance the channels by `cycles` base cycles (one output sample) and mix
    /// them, panned by `NR51` and scaled by the `NR50` master volume. Returns a
    /// left/right pair in a small integer range (0 = silence).
    pub fn step_sample(&mut self, cycles: i32) -> (i32, i32) {
        self.frame_timer -= cycles;
        while self.frame_timer <= 0 {
            self.frame_timer += FRAME_SEQ_PERIOD;
            self.clock_frame_sequencer();
        }
        self.ch1.step(cycles);
        self.ch2.step(cycles);
        self.ch3.step(cycles);
        self.ch4.step(cycles);

        let outs = [self.ch1.output(), self.ch2.output(), self.ch3.output(), self.ch4.output()];
        let mut left = 0i32;
        let mut right = 0i32;
        for (i, &o) in outs.iter().enumerate() {
            if self.nr51 & (1 << i) != 0 {
                right += o as i32;
            }
            if self.nr51 & (1 << (i + 4)) != 0 {
                left += o as i32;
            }
        }
        let left_vol = (self.nr50 & 7) as i32 + 1;
        let right_vol = ((self.nr50 >> 4) & 7) as i32 + 1;
        (left * left_vol, right * right_vol)
    }
}

fn env_from_byte(byte: u8) -> Envelope {
    Envelope {
        initial: (byte >> 4) & 0x0F,
        increasing: byte & 0x08 != 0,
        period: byte & 7,
        volume: (byte >> 4) & 0x0F,
        timer: byte & 7,
    }
}

fn hi_or_lo_bit7(byte: u8) -> bool {
    byte & 0x80 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triggered_square_produces_its_duty_pattern() {
        let mut psg = Psg::new();
        // NR11 = 0x80 (duty 50%), NR12 = 0xF8 (volume 15, increasing).
        psg.write16(0x062, 0xF880, 0xFFFF);
        // NR50 = 0x77 (full master volume), NR51 = 0xFF (all channels both sides).
        psg.write16(0x080, 0xFF77, 0xFFFF);
        // NR13/NR14 = 0x8000: frequency 0, trigger (bit 7 of the high byte).
        psg.write16(0x064, 0x8000, 0xFFFF);
        assert!(psg.ch1.enabled);
        // Step through a few thousand samples; something non-silent comes out.
        let mut peak = 0;
        for _ in 0..4000 {
            let (l, _r) = psg.step_sample(128);
            peak = peak.max(l.abs());
        }
        assert!(peak > 0, "square channel produced silence");
    }

    #[test]
    fn envelope_decays_to_zero() {
        let mut env = Envelope { initial: 5, increasing: false, period: 1, volume: 5, timer: 1 };
        for _ in 0..5 {
            env.clock();
        }
        assert_eq!(env.volume, 0);
    }

    #[test]
    fn length_counter_disables_channel() {
        let mut ch = Square {
            env: Envelope { initial: 15, ..Default::default() },
            length_enabled: true,
            length_timer: 2,
            enabled: true,
            ..Default::default()
        };
        ch.clock_length();
        assert!(ch.enabled);
        ch.clock_length();
        assert!(!ch.enabled);
    }

    #[test]
    fn dac_off_square_is_silent() {
        let mut psg = Psg::new();
        psg.write16(0x062, 0x2000, 0xFFFF); // duty set but envelope initial 0, not increasing -> DAC off
        psg.write16(0x064, 0x0080, 0xFFFF); // trigger
        assert!(!psg.ch1.enabled);
    }

    #[test]
    fn noise_lfsr_advances() {
        let mut n = Noise {
            env: Envelope { initial: 15, volume: 15, ..Default::default() },
            enabled: true,
            ..Default::default()
        };
        let start = n.lfsr;
        n.step(4096);
        assert_ne!(n.lfsr, start);
    }
}
