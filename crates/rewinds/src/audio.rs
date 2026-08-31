//! Host audio output: the cpal glue around the emulator's audio pipeline.
//!
//! The resampling and lock-free ring live in the `emulator` crate (so a Swift
//! CoreAudio backend can reuse them through the FFI); this file only opens the
//! default output device and drains the emulator's [`AudioSource`] from the cpal
//! callback. Underruns fill with silence, so audio simply goes quiet whenever the
//! emulator isn't running (pause/step/rewind keep working).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use emulator::Emulator;

/// A running host-audio output stream. Dropping it stops playback.
pub struct Audio {
    // Kept alive for the lifetime of `Audio`; never sent across threads.
    _stream: cpal::Stream,
}

impl Audio {
    /// Open the default stereo output device, attach it to `emulator`, and start
    /// playing. Returns `None` (with a note) when no suitable device/format
    /// exists — the emulator still runs, silently.
    pub fn open(emulator: &mut Emulator) -> Option<Audio> {
        let device = cpal::default_host().default_output_device()?;
        let config = device.default_output_config().ok()?;
        let dst_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        if channels != 2 {
            log::warn!("audio: default output is not stereo ({channels} channels); running silent");
            return None;
        }
        if config.sample_format() != cpal::SampleFormat::F32 {
            log::warn!("audio: unsupported sample format {:?}; running silent", config.sample_format());
            return None;
        }

        let mut source = emulator.enable_audio(dst_rate, channels);
        let stream = device
            .build_output_stream(
                &config.into(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let got = source.read(data);
                    data[got..].fill(0.0); // underrun -> silence
                },
                |e| log::error!("audio stream error: {e}"),
                None,
            )
            .ok()?;
        stream.play().ok()?;

        log::info!("audio: {dst_rate} Hz stereo output");
        Some(Audio { _stream: stream })
    }
}
