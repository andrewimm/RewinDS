//! Host audio output.
//!
//! The emulator produces interleaved stereo `i16` samples at a fixed source rate
//! ([`SRC_RATE`]); the host output device consumes at its own rate on its own
//! clock. We bridge them with a lock-free SPSC ring feeding a cpal callback, and
//! a **producer-side dynamic resampler**: each frame we resample the emulator's
//! samples to the host rate, nudging the ratio by ±0.5% to hold the ring near
//! half-full. That keeps playback gap-free without frame-skipping — the run loop
//! stays the master clock (so pause/step/rewind still work) and audio simply
//! underruns to silence when the emulator isn't running.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapProd, HeapRb};

/// Emulator output rate — matches `gba::apu::SAMPLE_RATE`.
const SRC_RATE: f64 = 32_768.0;

/// A streaming linear resampler for interleaved stereo, carrying its fractional
/// position across calls so successive chunks join seamlessly.
struct Resampler {
    /// Fractional position toward the next input frame, in input frames.
    pos: f64,
    /// The previous input frame, for interpolation continuity across chunks.
    last: [f32; 2],
}

impl Resampler {
    fn new() -> Self {
        Resampler { pos: 0.0, last: [0.0; 2] }
    }

    /// Resample `input` (interleaved stereo `i16`) into `out` (interleaved stereo
    /// `f32` in `-1.0..1.0`), emitting one output frame every `ratio` input frames.
    fn process(&mut self, input: &[i16], ratio: f64, out: &mut Vec<f32>) {
        for k in 0..input.len() / 2 {
            let cur = [input[k * 2] as f32 / 32768.0, input[k * 2 + 1] as f32 / 32768.0];
            while self.pos < 1.0 {
                let f = self.pos as f32;
                out.push(self.last[0] + (cur[0] - self.last[0]) * f);
                out.push(self.last[1] + (cur[1] - self.last[1]) * f);
                self.pos += ratio;
            }
            self.pos -= 1.0;
            self.last = cur;
        }
    }
}

/// A running host-audio output stream fed by [`Audio::push`].
pub struct Audio {
    // The stream is kept alive for the lifetime of `Audio`; dropping it stops
    // playback. It is never sent across threads.
    _stream: cpal::Stream,
    producer: HeapProd<f32>,
    resampler: Resampler,
    dst_rate: f64,
    ring_capacity: usize,
    scratch: Vec<f32>,
}

impl Audio {
    /// Open the default stereo output device and start playing. Returns `None`
    /// (with a note) when no suitable device/format exists — the emulator still
    /// runs, silently.
    pub fn new() -> Option<Audio> {
        let device = cpal::default_host().default_output_device()?;
        let config = device.default_output_config().ok()?;
        let dst_rate = config.sample_rate().0 as f64;
        let channels = config.channels() as usize;
        if channels != 2 {
            eprintln!("audio: default output is not stereo ({channels} channels); running silent");
            return None;
        }
        if config.sample_format() != cpal::SampleFormat::F32 {
            eprintln!("audio: unsupported sample format {:?}; running silent", config.sample_format());
            return None;
        }

        // Roughly a quarter-second of ring, so a slow frame cannot underrun.
        let ring_capacity = (dst_rate as usize) * channels / 4;
        let (producer, mut consumer) = HeapRb::<f32>::new(ring_capacity).split();

        let stream = device
            .build_output_stream(
                &config.into(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let got = consumer.pop_slice(data);
                    data[got..].fill(0.0); // underrun -> silence
                },
                |e| eprintln!("audio stream error: {e}"),
                None,
            )
            .ok()?;
        stream.play().ok()?;

        eprintln!("audio: {dst_rate} Hz stereo output");
        Some(Audio {
            _stream: stream,
            producer,
            resampler: Resampler::new(),
            dst_rate,
            ring_capacity,
            scratch: Vec::new(),
        })
    }

    /// Feed one frame of emulator samples (interleaved stereo `i16`), resampling
    /// to the host rate and nudging the rate to hold the ring near half-full.
    pub fn push(&mut self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let fill = self.producer.occupied_len();
        let target = self.ring_capacity / 2;
        // Ring too full -> consume more input per output (fewer outputs) so it
        // drains; too empty -> the reverse. Clamped to an inaudible ±0.5%.
        let adjust =
            ((fill as f64 - target as f64) / self.ring_capacity as f64 * 0.5).clamp(-0.005, 0.005);
        let ratio = (SRC_RATE / self.dst_rate) * (1.0 + adjust);
        self.scratch.clear();
        self.resampler.process(samples, ratio, &mut self.scratch);
        self.producer.push_slice(&self.scratch); // overflow is dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_output_count_tracks_ratio() {
        let mut r = Resampler::new();
        let mut out = Vec::new();
        let input: Vec<i16> = (0..2000).map(|i| (i % 97) as i16).collect(); // 1000 frames
        r.process(&input, SRC_RATE / 48_000.0, &mut out); // upsample to 48 kHz
        let frames = out.len() / 2;
        // 1000 * 48000/32768 ≈ 1465 output frames.
        assert!((frames as i64 - 1465).abs() <= 3, "got {frames}");
    }

    #[test]
    fn resampler_is_continuous_across_chunks() {
        // Splitting the input into two chunks yields ~the same output count as one.
        let input: Vec<i16> = (0..2000).map(|i| (i % 97) as i16).collect();
        let mut whole = Resampler::new();
        let mut a = Vec::new();
        whole.process(&input, 0.7, &mut a);
        let mut split = Resampler::new();
        let mut b = Vec::new();
        split.process(&input[..1000], 0.7, &mut b);
        split.process(&input[1000..], 0.7, &mut b);
        assert!((a.len() as i64 - b.len() as i64).abs() <= 2);
    }
}
