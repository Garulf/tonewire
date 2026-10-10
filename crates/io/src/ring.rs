use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

/// Counters shared between a stream and the control thread.
#[derive(Debug, Default)]
pub struct RingStats {
    pub underruns: AtomicU32,
    pub overruns: AtomicU32,
    pub corrections: AtomicU32,
}

impl RingStats {
    pub fn xruns(&self) -> u32 {
        self.underruns.load(Ordering::Relaxed) + self.overruns.load(Ordering::Relaxed)
    }
}

/// Creates an interleaved ring of `channels` channels that the reader keeps
/// filled to about `target_frames`, sized for `max_block` frames per call.
pub fn ring(channels: usize, target_frames: usize, max_block: usize) -> (RingWriter, RingReader) {
    let capacity = (target_frames + 8 * max_block.max(1)) * channels;
    let (producer, consumer) = RingBuffer::new(capacity);
    let stats = Arc::new(RingStats::default());
    (
        RingWriter {
            producer,
            channels,
            stats: stats.clone(),
        },
        RingReader {
            consumer,
            channels,
            target: target_frames,
            band: (target_frames / 2).max(32),
            started: false,
            average_fill: target_frames as f64,
            scratch: vec![0.0; (max_block + 2) * channels],
            stats,
        },
    )
}

pub struct RingWriter {
    producer: Producer<f32>,
    channels: usize,
    stats: Arc<RingStats>,
}

impl RingWriter {
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn stats(&self) -> &Arc<RingStats> {
        &self.stats
    }

    /// Writes `frames` frames, asking `frame` to fill each interleaved frame.
    /// Frames that do not fit are dropped and counted as an overrun.
    pub fn write(&mut self, frames: usize, mut frame: impl FnMut(usize, &mut [f32])) {
        let fit = (self.producer.slots() / self.channels).min(frames);
        if fit < frames {
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
        if fit == 0 {
            return;
        }
        let Ok(mut chunk) = self.producer.write_chunk(fit * self.channels) else {
            return;
        };
        let (a, b) = chunk.as_mut_slices();
        let mut index = 0;
        let mut tmp = [0.0f32; nodeaudio_graph::MAX_CHANNELS];
        for i in 0..fit {
            let out = &mut tmp[..self.channels];
            frame(i, out);
            for &s in out.iter() {
                if index < a.len() {
                    a[index] = s;
                } else {
                    b[index - a.len()] = s;
                }
                index += 1;
            }
        }
        chunk.commit_all();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// Reader and writer share a clock and run back to back (clock master).
    Direct,
    /// Independently clocked writer: prefill, then slip frames to hold the
    /// fill near the target.
    Corrected,
}

pub struct RingReader {
    consumer: Consumer<f32>,
    channels: usize,
    target: usize,
    band: usize,
    started: bool,
    average_fill: f64,
    scratch: Vec<f32>,
    stats: Arc<RingStats>,
}

impl RingReader {
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn stats(&self) -> &Arc<RingStats> {
        &self.stats
    }

    pub fn fill(&self) -> usize {
        self.consumer.slots() / self.channels
    }

    /// Reads `frames` frames, handing each interleaved frame to `frame`.
    /// Returns `false` (calling nothing) while there is no audio to deliver.
    pub fn read(
        &mut self,
        frames: usize,
        mode: ReadMode,
        mut frame: impl FnMut(usize, &[f32]),
    ) -> bool {
        let available = self.fill();
        if mode == ReadMode::Direct {
            if available < frames {
                if available > 0 || self.started {
                    self.stats.underruns.fetch_add(1, Ordering::Relaxed);
                }
                self.discard(available);
                return false;
            }
            self.started = true;
            self.copy_out(frames, frames, &mut frame);
            return true;
        }

        if !self.started {
            if available < self.target + frames {
                return false;
            }
            self.started = true;
            self.average_fill = (available - frames) as f64;
        }
        if available < frames {
            self.stats.underruns.fetch_add(1, Ordering::Relaxed);
            self.started = false;
            self.discard(available);
            return false;
        }
        if available > self.target + 8 * self.band + frames {
            self.discard(available - self.target - frames);
            self.average_fill = self.target as f64;
            self.stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
        let level = self.fill() - frames;
        self.average_fill += (level as f64 - self.average_fill) * 0.02;
        let high = self.average_fill > (self.target + self.band) as f64 && level > self.target;
        let low =
            self.average_fill < self.target.saturating_sub(self.band) as f64 && level < self.target;
        let take = if high && frames > 1 {
            frames + 1
        } else if low && frames > 1 {
            frames - 1
        } else {
            frames
        };
        if take != frames {
            self.stats.corrections.fetch_add(1, Ordering::Relaxed);
        }
        self.copy_out(take, frames, &mut frame);
        true
    }

    fn discard(&mut self, frames: usize) {
        if let Ok(chunk) = self.consumer.read_chunk(frames * self.channels) {
            chunk.commit_all();
        }
    }

    /// Consumes `take` frames and emits `frames` frames, linearly stretching
    /// when the two differ by the one-frame slip.
    fn copy_out(&mut self, take: usize, frames: usize, frame: &mut impl FnMut(usize, &[f32])) {
        let channels = self.channels;
        let take = take.min(self.scratch.len() / channels);
        let Ok(chunk) = self.consumer.read_chunk(take * channels) else {
            return;
        };
        let (a, b) = chunk.as_slices();
        self.scratch[..a.len()].copy_from_slice(a);
        self.scratch[a.len()..a.len() + b.len()].copy_from_slice(b);
        chunk.commit_all();

        if take == frames {
            for i in 0..frames {
                frame(i, &self.scratch[i * channels..(i + 1) * channels]);
            }
            return;
        }
        let mut out = [0.0f32; nodeaudio_graph::MAX_CHANNELS];
        let step = (take - 1) as f64 / (frames - 1).max(1) as f64;
        for i in 0..frames {
            let pos = i as f64 * step;
            let at = (pos as usize).min(take - 1);
            let next = (at + 1).min(take - 1);
            let t = (pos - at as f64) as f32;
            let (a, b) = (
                &self.scratch[at * channels..],
                &self.scratch[next * channels..],
            );
            for ((o, &x0), &x1) in out.iter_mut().zip(a).zip(b).take(channels) {
                *o = x0 + (x1 - x0) * t;
            }
            frame(i, &out[..channels]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_mode_passes_samples_in_order() {
        let (mut w, mut r) = ring(2, 0, 64);
        w.write(4, |i, f| f.fill(i as f32));
        let mut got = Vec::new();
        assert!(r.read(4, ReadMode::Direct, |_, f| got.push(f[1])));
        assert_eq!(got, [0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn corrected_mode_waits_for_prefill_and_counts_underruns() {
        let (mut w, mut r) = ring(1, 128, 128);
        w.write(128, |_, f| f[0] = 1.0);
        assert!(!r.read(128, ReadMode::Corrected, |_, _| {}));
        w.write(128, |_, f| f[0] = 1.0);
        assert!(r.read(128, ReadMode::Corrected, |_, _| {}));
        assert!(r.read(128, ReadMode::Corrected, |_, _| {}));
        assert!(!r.read(128, ReadMode::Corrected, |_, _| {}));
        assert_eq!(r.stats().underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn full_ring_drops_and_counts_overrun() {
        let (mut w, _r) = ring(1, 0, 1);
        w.write(100, |_, f| f[0] = 0.0);
        assert_eq!(w.stats().overruns.load(Ordering::Relaxed), 1);
    }

    /// Writer runs `ppm` parts per million faster or slower than the reader.
    fn simulate_drift(ppm: f64) -> (usize, usize, u32) {
        let block = 128;
        let target = 256;
        let (mut w, mut r) = ring(1, target, block);
        let mut owed = 0.0f64;
        let (mut min_fill, mut max_fill) = (usize::MAX, 0);
        let blocks = 48_000 * 60 / block;
        for n in 0..blocks {
            owed += block as f64 * (1.0 + ppm * 1e-6);
            let whole = owed.floor() as usize;
            owed -= whole as f64;
            w.write(whole, |_, f| f[0] = 0.1);
            r.read(block, ReadMode::Corrected, |_, _| {});
            if n > blocks / 10 {
                min_fill = min_fill.min(r.fill());
                max_fill = max_fill.max(r.fill());
            }
        }
        (
            min_fill,
            max_fill,
            r.stats().underruns.load(Ordering::Relaxed),
        )
    }

    #[test]
    fn drift_correction_keeps_fill_bounded() {
        for ppm in [-200.0, 0.0, 200.0] {
            let (min, max, underruns) = simulate_drift(ppm);
            assert_eq!(underruns, 0, "{ppm} ppm underran");
            assert!(
                min >= 64 && max <= 640,
                "{ppm} ppm fill ranged {min}..{max}"
            );
        }
    }

    #[test]
    fn stretch_has_no_jumps() {
        let (mut w, mut r) = ring(1, 0, 16);
        w.write(17, |i, f| f[0] = i as f32);
        let mut out = Vec::new();
        r.copy_out(17, 16, &mut |_, f: &[f32]| out.push(f[0]));
        assert_eq!(out.first(), Some(&0.0));
        assert_eq!(out.last(), Some(&16.0));
        assert!(out.windows(2).all(|p| p[1] > p[0] && p[1] - p[0] < 1.1));
    }
}
