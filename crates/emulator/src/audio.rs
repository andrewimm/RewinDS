//! The host-agnostic audio output pipeline: resampling plus a lock-free SPSC ring.
//!
//! The emulator produces interleaved stereo `i16` at a fixed source rate
//! ([`SRC_RATE`], matching `gba::apu::SAMPLE_RATE`); the host consumes `f32` at
//! its own rate, on its own audio thread. The two halves are split so each stays
//! single-thread-affine: the emulator pushes into an [`AudioSink`] (producer)
//! from its run loop, and the host drains an [`AudioSource`] (consumer) it has
//! moved into its audio callback. This is the shape a C-FFI wants — one opaque
//! producer bound to the emulator handle, one opaque consumer the host owns.
//!
//! A **producer-side dynamic resampler** nudges the resample ratio by ±0.5% each
//! frame to hold the ring near half-full, so playback stays gap-free without
//! frame-skipping: the run loop remains the master clock (pause/step/rewind still
//! work) and audio simply underruns to silence when the emulator isn't running.

use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

/// Emulator output rate — matches `gba::apu::SAMPLE_RATE`.
pub const SRC_RATE: f64 = 32_768.0;

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

/// Create a connected producer/consumer pair for a host output stream running at
/// `output_rate` Hz with `channels` channels. The ring holds roughly a quarter
/// second, so a slow frame cannot underrun.
pub fn channel(output_rate: u32, channels: usize) -> (AudioSink, AudioSource) {
    let ring_capacity = (output_rate as usize) * channels / 4;
    let (producer, consumer) = HeapRb::<f32>::new(ring_capacity).split();
    (
        AudioSink {
            producer,
            resampler: Resampler::new(),
            dst_rate: output_rate as f64,
            ring_capacity,
            scratch: Vec::new(),
        },
        AudioSource { consumer },
    )
}

/// The producer half, driven from the emulator's run loop. Bound to the emulator
/// handle and never sent across threads.
pub struct AudioSink {
    producer: HeapProd<f32>,
    resampler: Resampler,
    dst_rate: f64,
    ring_capacity: usize,
    scratch: Vec<f32>,
}

impl AudioSink {
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

/// The consumer half. The host moves this into its audio callback and drains it;
/// it is `Send` so it can cross to the audio thread.
pub struct AudioSource {
    consumer: HeapCons<f32>,
}

impl AudioSource {
    /// Drain up to `out.len()` `f32` samples (interleaved stereo) into `out`,
    /// returning how many were written. The caller fills the remainder with
    /// silence on an underrun.
    pub fn read(&mut self, out: &mut [f32]) -> usize {
        self.consumer.pop_slice(out)
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

    #[test]
    fn sink_pushes_and_source_drains() {
        let (mut sink, mut source) = channel(48_000, 2);
        let input: Vec<i16> = (0..2048).map(|i| (i % 50) as i16).collect();
        sink.push(&input);
        let mut out = vec![0.0f32; 4096];
        let got = source.read(&mut out);
        assert!(got > 0, "expected resampled samples to be available");
    }
}
