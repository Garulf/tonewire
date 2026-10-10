use std::ops::{Deref, DerefMut};

use crate::MAX_CHANNELS;

/// Planar audio: one boxed slice per channel, each `max_block` long.
#[derive(Debug, Clone)]
pub struct AudioBuf {
    channels: Vec<Box<[f32]>>,
    silent: bool,
}

pub struct ChannelSlices<'a> {
    slices: [&'a mut [f32]; MAX_CHANNELS],
    len: usize,
}

impl<'a> Deref for ChannelSlices<'a> {
    type Target = [&'a mut [f32]];
    fn deref(&self) -> &Self::Target {
        &self.slices[..self.len]
    }
}

impl DerefMut for ChannelSlices<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.slices[..self.len]
    }
}

impl AudioBuf {
    pub fn new(channels: usize, max_block: usize) -> Self {
        let channels = channels.clamp(1, MAX_CHANNELS);
        Self {
            channels: (0..channels)
                .map(|_| vec![0.0; max_block].into_boxed_slice())
                .collect(),
            silent: true,
        }
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    pub fn capacity(&self) -> usize {
        self.channels.first().map_or(0, |c| c.len())
    }

    pub fn is_silent(&self) -> bool {
        self.silent
    }

    pub fn set_silent(&mut self, silent: bool) {
        self.silent = silent;
    }

    pub fn channel(&self, index: usize, frames: usize) -> &[f32] {
        &self.channels[index][..frames]
    }

    pub fn channel_mut(&mut self, index: usize, frames: usize) -> &mut [f32] {
        self.silent = false;
        &mut self.channels[index][..frames]
    }

    pub fn slices_mut(&mut self, frames: usize) -> ChannelSlices<'_> {
        self.silent = false;
        let mut slices: [&mut [f32]; MAX_CHANNELS] = Default::default();
        let len = self.channels.len();
        for (slot, channel) in slices.iter_mut().zip(self.channels.iter_mut()) {
            *slot = &mut channel[..frames];
        }
        ChannelSlices { slices, len }
    }

    pub fn clear(&mut self, frames: usize) {
        if !self.silent {
            for c in &mut self.channels {
                c[..frames].fill(0.0);
            }
            self.silent = true;
        }
    }

    pub fn scale(&mut self, frames: usize, gain: f32) {
        if self.silent || gain == 1.0 {
            return;
        }
        for c in &mut self.channels {
            c[..frames].iter_mut().for_each(|s| *s *= gain);
        }
    }

    pub fn ramp(&mut self, frames: usize, from: f32, to: f32) {
        if self.silent || (from == 1.0 && to == 1.0) || frames == 0 {
            return;
        }
        let step = (to - from) / frames as f32;
        for c in &mut self.channels {
            for (i, s) in c[..frames].iter_mut().enumerate() {
                *s *= from + step * i as f32;
            }
        }
    }

    pub fn peak_rms(&self, frames: usize) -> (f32, f32) {
        if self.silent || frames == 0 {
            return (0.0, 0.0);
        }
        let mut peak = 0.0f32;
        let mut sum = 0.0f32;
        for c in &self.channels {
            for &s in &c[..frames] {
                peak = peak.max(s.abs());
                sum += s * s;
            }
        }
        (peak, (sum / (frames * self.channels.len()) as f32).sqrt())
    }

    /// Writes (or adds, when `add` is set) `src * gain` into `self`, adapting
    /// channel counts: mono is duplicated, a mono destination gets the average,
    /// otherwise channels map by index and missing ones stay silent.
    pub fn mix_from(&mut self, src: &AudioBuf, frames: usize, gain: f32, add: bool) {
        if !add {
            self.clear(frames);
        }
        if src.silent {
            return;
        }
        if self.silent {
            for c in &mut self.channels {
                c[..frames].fill(0.0);
            }
        }
        self.silent = false;
        let (dst_n, src_n) = (self.channels.len(), src.channels.len());
        if dst_n == 1 && src_n > 1 {
            let g = gain / src_n as f32;
            let dst = &mut self.channels[0][..frames];
            for s in &src.channels {
                for (d, &x) in dst.iter_mut().zip(&s[..frames]) {
                    *d += x * g;
                }
            }
            return;
        }
        for (i, dst) in self.channels.iter_mut().enumerate() {
            let source = if src_n == 1 {
                &src.channels[0]
            } else if let Some(s) = src.channels.get(i) {
                s
            } else {
                continue;
            };
            for (d, &x) in dst[..frames].iter_mut().zip(&source[..frames]) {
                *d += x * gain;
            }
        }
    }
}

/// Fixed delay used to line up branches with different processing latency.
#[derive(Debug, Clone)]
pub struct DelayLine {
    history: AudioBuf,
    scratch: AudioBuf,
    position: usize,
    delay: usize,
}

impl DelayLine {
    pub fn new(channels: usize, delay: usize, max_block: usize) -> Self {
        Self {
            history: AudioBuf::new(channels, delay.max(1)),
            scratch: AudioBuf::new(channels, max_block),
            position: 0,
            delay,
        }
    }

    pub fn delay(&self) -> usize {
        self.delay
    }

    pub fn scratch_mut(&mut self) -> &mut AudioBuf {
        &mut self.scratch
    }

    /// Delays `scratch` in place and returns it.
    pub fn process_scratch(&mut self, frames: usize) -> &AudioBuf {
        let delay = self.delay;
        let scratch_silent = self.scratch.is_silent();
        if scratch_silent && self.history.is_silent() {
            return &self.scratch;
        }
        if scratch_silent {
            for c in 0..self.scratch.channel_count() {
                self.scratch.channels[c][..frames].fill(0.0);
            }
        }
        let mut position = self.position;
        for c in 0..self.scratch.channel_count() {
            position = self.position;
            let history = &mut self.history.channels[c];
            for s in &mut self.scratch.channels[c][..frames] {
                std::mem::swap(&mut history[position], s);
                position += 1;
                if position == delay {
                    position = 0;
                }
            }
        }
        self.position = position;
        self.history.silent = false;
        self.scratch.silent = false;
        &self.scratch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(channels: usize, values: &[f32]) -> AudioBuf {
        let mut b = AudioBuf::new(channels, values.len());
        for c in 0..channels {
            b.channel_mut(c, values.len()).copy_from_slice(values);
        }
        b
    }

    #[test]
    fn mono_to_stereo_duplicates_and_stereo_to_mono_averages() {
        let mono = filled(1, &[1.0, 2.0]);
        let mut stereo = AudioBuf::new(2, 2);
        stereo.mix_from(&mono, 2, 1.0, false);
        assert_eq!(stereo.channel(1, 2), &[1.0, 2.0]);

        let mut down = AudioBuf::new(1, 2);
        let mut src = AudioBuf::new(2, 2);
        src.channel_mut(0, 2).copy_from_slice(&[1.0, 1.0]);
        src.channel_mut(1, 2).copy_from_slice(&[3.0, 3.0]);
        down.mix_from(&src, 2, 1.0, false);
        assert_eq!(down.channel(0, 2), &[2.0, 2.0]);
    }

    #[test]
    fn adding_sums_sources() {
        let a = filled(2, &[1.0, 1.0]);
        let b = filled(2, &[0.5, 0.5]);
        let mut dst = AudioBuf::new(2, 2);
        dst.mix_from(&a, 2, 1.0, false);
        dst.mix_from(&b, 2, 2.0, true);
        assert_eq!(dst.channel(0, 2), &[2.0, 2.0]);
    }

    #[test]
    fn silent_source_clears_destination() {
        let mut dst = filled(1, &[1.0, 1.0]);
        dst.mix_from(&AudioBuf::new(1, 2), 2, 1.0, false);
        assert!(dst.is_silent());
        assert_eq!(dst.channel(0, 2), &[0.0, 0.0]);
    }

    #[test]
    fn delay_line_shifts_by_delay_across_blocks() {
        let mut d = DelayLine::new(1, 3, 4);
        let mut out = Vec::new();
        for block in [[1.0, 2.0, 3.0, 4.0], [5.0, 6.0, 7.0, 8.0]] {
            d.scratch_mut().channel_mut(0, 4).copy_from_slice(&block);
            out.extend_from_slice(d.process_scratch(4).channel(0, 4));
        }
        assert_eq!(out, [0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    }
}
