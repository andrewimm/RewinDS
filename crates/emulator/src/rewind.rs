//! Time-travel debugging: a rewind timeline.
//!
//! The emulator is deterministic — a frame's result is fixed by the prior state and the input
//! applied — so we don't store a snapshot per frame (that would be gigabytes). Instead the
//! [`Timeline`] records the input used every frame and takes a full-state **keyframe** snapshot
//! only every `keyframe_interval` frames. Any past frame is reconstructed by restoring the
//! nearest earlier keyframe and replaying the recorded inputs forward to it.
//!
//! Frame numbers are the emulator's own ([`Emulator::frame`]); a snapshot restores that counter
//! along with everything else, so the timeline, `seek` targets, and the emulator always agree.
//!
//! This is opt-in: a `Timeline` exists only while rewind is enabled, so it costs nothing
//! otherwise. Memory is bounded — roughly `keyframe_interval * capacity` frames of history,
//! at one `System` clone (~5 MB on the DS) per retained keyframe — and both knobs are the
//! host's to choose against its memory budget.
//!
//! Determinism caveat: replay reproduces a frame exactly only if nothing outside the recorded
//! input and the snapshotted state influences it. Both consoles' clocks are captured — the DS
//! RTC lives in the snapshot, and the GBA cartridge RTC derives its time from the (snapshotted)
//! scheduler clock plus a base latched at load.

use crate::{Emulator, Input};
use std::collections::VecDeque;

/// One retained keyframe: the emulator frame it captures and the full machine state there.
struct Keyframe {
    frame: u64,
    snapshot: crate::Snapshot,
}

/// A rewind recorder. The host drives it once per frame with [`Timeline::record`] (after the
/// frame has run); [`Timeline::seek`] then reconstructs any recorded frame. Seeking leaves the
/// emulator at that past frame for inspection; running and recording a new frame from there
/// branches the timeline (the superseded future is dropped).
pub struct Timeline {
    keyframe_interval: u64,
    capacity: usize,
    /// Keyframes in increasing frame order; `front` is the oldest reachable.
    keyframes: VecDeque<Keyframe>,
    /// Per-frame inputs. `inputs[i]` is the input in effect during frame `input_base + i`.
    /// Trimmed from the front so it never starts later than the oldest keyframe.
    inputs: VecDeque<Input>,
    input_base: u64,
    /// The newest recorded frame (the live edge of the timeline).
    newest: u64,
    /// The frame the emulator is currently at (equals `newest` during live play; less after a
    /// [`Timeline::seek`] into the past).
    cursor: u64,
    started: bool,
}

impl Timeline {
    /// A new, empty timeline. `keyframe_interval` frames between snapshots (clamped to ≥ 1);
    /// `capacity` keyframes retained (clamped to ≥ 1). History depth is about
    /// `keyframe_interval * capacity` frames; a seek replays at most `keyframe_interval`
    /// frames, so a larger interval trades slower seeks for less memory.
    pub fn new(keyframe_interval: u32, capacity: usize) -> Self {
        Timeline {
            keyframe_interval: keyframe_interval.max(1) as u64,
            capacity: capacity.max(1),
            keyframes: VecDeque::new(),
            inputs: VecDeque::new(),
            input_base: 0,
            newest: 0,
            cursor: 0,
            started: false,
        }
    }

    /// Record the frame the emulator just produced (its [`Emulator::frame`]) under the input
    /// that drove it. Call once per `run_frame`, immediately after it returns. Takes a keyframe
    /// when the frame lands on the interval (and always for the first), and trims history beyond
    /// `capacity`. If the emulator was rewound into the past, the superseded future is dropped
    /// first (a new branch). A repeated call at the same frame is a no-op.
    pub fn record(&mut self, emu: &Emulator) {
        let f = emu.frame();
        if self.started && f == self.cursor {
            return; // no new frame advanced since the last record
        }
        // Resuming play after a rewind: the emulator is at an earlier frame; drop the future.
        if self.started && f <= self.newest {
            self.truncate_after(f.saturating_sub(1));
        }
        if !self.started {
            self.input_base = f;
            self.started = true;
        }
        self.inputs.push_back(emu.last_input());
        self.newest = f;
        self.cursor = f;
        if self.keyframes.is_empty() || f.is_multiple_of(self.keyframe_interval) {
            self.keyframes.push_back(Keyframe { frame: f, snapshot: emu.snapshot() });
            while self.keyframes.len() > self.capacity {
                self.keyframes.pop_front();
            }
            // Inputs older than the oldest keyframe can never be replayed; drop them.
            let oldest = self.keyframes.front().map_or(f, |k| k.frame);
            while self.input_base < oldest && self.inputs.len() > 1 {
                self.inputs.pop_front();
                self.input_base += 1;
            }
        }
    }

    /// Reconstruct `target` (clamped to the recorded range) in `emu`: restore the nearest
    /// keyframe at or before it, then replay the recorded inputs forward. Leaves the emulator
    /// at `target` and returns the frame actually reached.
    pub fn seek(&mut self, emu: &mut Emulator, target: u64) -> u64 {
        if !self.started {
            return self.cursor;
        }
        let target = target.clamp(self.oldest_frame(), self.newest);
        let kf = self
            .keyframes
            .iter()
            .rev()
            .find(|k| k.frame <= target)
            .unwrap_or_else(|| self.keyframes.front().unwrap());
        emu.restore(&kf.snapshot);
        // Replay to the target: each step applies the input in effect during the next frame.
        while emu.frame() < target {
            let next = emu.frame() + 1;
            if let Some(input) = self.inputs.get((next - self.input_base) as usize) {
                emu.set_input(*input);
            }
            emu.run_frame();
        }
        self.cursor = emu.frame();
        self.cursor
    }

    /// Step one frame into the past (`seek(cursor - 1)`); returns the frame reached.
    pub fn step_back(&mut self, emu: &mut Emulator) -> u64 {
        let target = self.cursor.saturating_sub(1);
        self.seek(emu, target)
    }

    /// The oldest frame that can be reconstructed (the oldest retained keyframe).
    pub fn oldest_frame(&self) -> u64 {
        self.keyframes.front().map_or(self.cursor, |k| k.frame)
    }

    /// The newest recorded frame (the live edge).
    pub fn newest_frame(&self) -> u64 {
        self.newest
    }

    /// Where the emulator is currently positioned (`newest_frame` during live play).
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// The frames at which keyframes are retained (oldest first) — the instant-restore points.
    pub fn keyframe_frames(&self) -> Vec<u64> {
        self.keyframes.iter().map(|k| k.frame).collect()
    }

    /// Retained-state size estimate in bytes (keyframe count × a representative snapshot size).
    pub fn approx_bytes(&self, per_snapshot: usize) -> usize {
        self.keyframes.len() * per_snapshot
    }

    /// Drop everything after frame `n` (keyframes and inputs), making `n` the live edge.
    fn truncate_after(&mut self, n: u64) {
        while self.keyframes.back().is_some_and(|k| k.frame > n) {
            self.keyframes.pop_back();
        }
        // inputs[i] is frame input_base + i; keep frames input_base..=n.
        let keep = if n >= self.input_base {
            (n - self.input_base + 1) as usize
        } else {
            0
        };
        self.inputs.truncate(keep.min(self.inputs.len()));
        self.newest = n;
    }
}
