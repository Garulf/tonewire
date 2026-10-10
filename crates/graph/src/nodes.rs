use tonewire_dsp::{
    Biquad, BiquadCoeffs, BiquadKind, Compressor, CompressorSettings, DeEsser, DeEsserSettings,
    Denoiser, Gate, GateSettings, Limiter, Smoothed, db_to_gain,
};

use crate::MAX_CHANNELS;
use crate::buffer::AudioBuf;
use crate::catalog::EQ_BANDS;
use crate::model::NodeKind;
use crate::processor::{NodeProcessor, ProcessCtx};
use crate::shared::NodeShared;

const GAIN_SMOOTHING_MS: f32 = 10.0;

/// Builds processors for node kinds this crate does not implement itself
/// (NVIDIA effects). Called on the control thread.
pub trait ExternalFactory: Send + Sync {
    /// Why `kind` cannot be created right now, if it cannot.
    fn unavailable(&self, kind: &NodeKind) -> Option<String> {
        let _ = kind;
        None
    }

    fn create(
        &self,
        kind: &NodeKind,
        channels: usize,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<Box<dyn NodeProcessor>, String>;
}

pub fn create(
    kind: &NodeKind,
    channels: usize,
    sample_rate: f32,
    max_block: usize,
    external: Option<&dyn ExternalFactory>,
) -> Result<Box<dyn NodeProcessor>, String> {
    Ok(match kind {
        NodeKind::DeviceInput | NodeKind::VirtualSpeaker | NodeKind::AppSource => {
            Box::new(Source::new(sample_rate))
        }
        NodeKind::DeviceOutput | NodeKind::VirtualMic => Box::new(Sink::new(sample_rate)),
        NodeKind::Gain => Box::new(Gain::new(sample_rate)),
        NodeKind::Pan => Box::new(Pan::new(sample_rate)),
        NodeKind::Mixer { inputs } => Box::new(Mixer::new(usize::from(*inputs), sample_rate)),
        NodeKind::Splitter { outputs } => {
            Box::new(Splitter::new(usize::from(*outputs), sample_rate))
        }
        NodeKind::Eq => Box::new(Eq::new(channels, sample_rate)),
        NodeKind::DeEsser => Box::new(DeEsserNode::new(channels, sample_rate)),
        NodeKind::Compressor => Box::new(CompressorNode::new(sample_rate)),
        NodeKind::Gate => Box::new(GateNode::new(sample_rate)),
        NodeKind::Limiter => Box::new(LimiterNode::new(sample_rate)),
        NodeKind::Denoise => Box::new(DenoiseNode::new(channels, max_block)),
        NodeKind::Meter => Box::new(PassThrough),
        NodeKind::NvAfx { .. } => match external {
            Some(factory) => factory.create(kind, channels, sample_rate, max_block)?,
            None => return Err("NVIDIA Audio Effects SDK is not available".into()),
        },
    })
}

/// Runs a node's settings refresh only when the control thread changed a param.
#[derive(Debug, Default)]
struct ParamWatch(Option<u32>);

impl ParamWatch {
    fn changed(&mut self, shared: &NodeShared) -> bool {
        let version = shared.version();
        let changed = self.0 != Some(version);
        self.0 = Some(version);
        changed
    }
}

fn level_gain(shared: &NodeShared) -> f32 {
    if shared.param(1) >= 0.5 {
        0.0
    } else {
        db_to_gain(shared.param(0))
    }
}

pub struct PassThrough;

impl NodeProcessor for PassThrough {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        ctx.pass_through();
    }
    fn skips_silence(&self) -> bool {
        true
    }
}

struct Source {
    gain: Smoothed,
}

impl Source {
    fn new(sample_rate: f32) -> Self {
        Self {
            gain: Smoothed::new(1.0, GAIN_SMOOTHING_MS, sample_rate),
        }
    }
}

impl NodeProcessor for Source {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        self.gain.set_target(level_gain(ctx.shared));
        let output = &mut ctx.outputs[0];
        if ctx.io.read(ctx.node, output, ctx.frames) {
            output.set_silent(false);
            apply_smoothed(&mut self.gain, output, ctx.frames);
        } else {
            output.clear(ctx.frames);
        }
    }
}

struct Sink {
    gain: Smoothed,
}

impl Sink {
    fn new(sample_rate: f32) -> Self {
        Self {
            gain: Smoothed::new(1.0, GAIN_SMOOTHING_MS, sample_rate),
        }
    }
}

impl NodeProcessor for Sink {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        self.gain.set_target(level_gain(ctx.shared));
        let input = &mut ctx.inputs[0];
        apply_smoothed(&mut self.gain, input, ctx.frames);
        input.ramp(ctx.frames, ctx.fade.0, ctx.fade.1);
        ctx.io.write(ctx.node, input, ctx.frames);
    }
}

fn apply_smoothed(gain: &mut Smoothed, buf: &mut AudioBuf, frames: usize) {
    if buf.is_silent() {
        if !gain.is_settled() {
            gain.jump_to(gain.target());
        }
        return;
    }
    if gain.is_settled() {
        buf.scale(frames, gain.target());
        return;
    }
    let channels = buf.channel_count();
    let start = gain.clone();
    for c in 0..channels {
        *gain = start.clone();
        gain.apply(buf.channel_mut(c, frames));
    }
}

struct Gain {
    gain: Smoothed,
}

impl Gain {
    fn new(sample_rate: f32) -> Self {
        Self {
            gain: Smoothed::new(1.0, GAIN_SMOOTHING_MS, sample_rate),
        }
    }
}

impl NodeProcessor for Gain {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        self.gain.set_target(level_gain(ctx.shared));
        apply_smoothed(&mut self.gain, &mut ctx.inputs[0], ctx.frames);
        ctx.pass_through();
    }
    fn skips_silence(&self) -> bool {
        true
    }
}

struct Pan {
    left: Smoothed,
    right: Smoothed,
}

impl Pan {
    fn new(sample_rate: f32) -> Self {
        let center = std::f32::consts::FRAC_1_SQRT_2;
        Self {
            left: Smoothed::new(center, GAIN_SMOOTHING_MS, sample_rate),
            right: Smoothed::new(center, GAIN_SMOOTHING_MS, sample_rate),
        }
    }
}

impl NodeProcessor for Pan {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        let angle = (ctx.shared.param(0) + 1.0) * std::f32::consts::FRAC_PI_4;
        let center = std::f32::consts::FRAC_1_SQRT_2;
        self.left.set_target(angle.cos() / center);
        self.right.set_target(angle.sin() / center);
        let input = &mut ctx.inputs[0];
        if input.channel_count() >= 2 && !input.is_silent() {
            self.left.apply(input.channel_mut(0, ctx.frames));
            self.right.apply(input.channel_mut(1, ctx.frames));
        }
        ctx.pass_through();
    }
    fn skips_silence(&self) -> bool {
        true
    }
}

struct Mixer {
    inputs: Vec<Smoothed>,
    output: Smoothed,
}

impl Mixer {
    fn new(inputs: usize, sample_rate: f32) -> Self {
        Self {
            inputs: (0..inputs)
                .map(|_| Smoothed::new(1.0, GAIN_SMOOTHING_MS, sample_rate))
                .collect(),
            output: Smoothed::new(1.0, GAIN_SMOOTHING_MS, sample_rate),
        }
    }
}

impl NodeProcessor for Mixer {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        let frames = ctx.frames;
        let output = &mut ctx.outputs[0];
        output.clear(frames);
        for (i, (input, gain)) in ctx.inputs.iter_mut().zip(&mut self.inputs).enumerate() {
            gain.set_target(db_to_gain(ctx.shared.param(i)));
            apply_smoothed(gain, input, frames);
            output.mix_from(input, frames, 1.0, true);
        }
        self.output
            .set_target(db_to_gain(ctx.shared.param(self.inputs.len())));
        apply_smoothed(&mut self.output, output, frames);
    }
    fn skips_silence(&self) -> bool {
        true
    }
}

struct Splitter {
    outputs: Vec<Smoothed>,
}

impl Splitter {
    fn new(outputs: usize, sample_rate: f32) -> Self {
        Self {
            outputs: (0..outputs)
                .map(|_| Smoothed::new(1.0, GAIN_SMOOTHING_MS, sample_rate))
                .collect(),
        }
    }
}

impl NodeProcessor for Splitter {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        let frames = ctx.frames;
        let input = &ctx.inputs[0];
        for (i, (output, gain)) in ctx.outputs.iter_mut().zip(&mut self.outputs).enumerate() {
            gain.set_target(db_to_gain(ctx.shared.param(i)));
            output.mix_from(input, frames, 1.0, false);
            apply_smoothed(gain, output, frames);
        }
    }
    fn skips_silence(&self) -> bool {
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct BandSettings {
    on: bool,
    kind: BiquadKind,
    freq: f32,
    gain_db: f32,
    q: f32,
}

fn eq_kind(index: f32) -> BiquadKind {
    match index as usize {
        0 => BiquadKind::LowShelf,
        2 => BiquadKind::HighShelf,
        3 => BiquadKind::LowPass,
        4 => BiquadKind::HighPass,
        5 => BiquadKind::Notch,
        6 => BiquadKind::BandPass,
        _ => BiquadKind::Peak,
    }
}

struct Eq {
    sample_rate: f32,
    watch: ParamWatch,
    settings: [Option<BandSettings>; EQ_BANDS],
    filters: [[Biquad; MAX_CHANNELS]; EQ_BANDS],
    channels: usize,
}

impl Eq {
    fn new(channels: usize, sample_rate: f32) -> Self {
        Self {
            sample_rate,
            watch: ParamWatch::default(),
            settings: [None; EQ_BANDS],
            filters: Default::default(),
            channels: channels.min(MAX_CHANNELS),
        }
    }

    fn refresh(&mut self, shared: &NodeShared) {
        for band in 0..EQ_BANDS {
            let base = band * 5;
            let settings = BandSettings {
                on: shared.param(base) >= 0.5,
                kind: eq_kind(shared.param(base + 1)),
                freq: shared.param(base + 2),
                gain_db: shared.param(base + 3),
                q: shared.param(base + 4),
            };
            if self.settings[band] == Some(settings) {
                continue;
            }
            let was_on = self.settings[band].is_some_and(|s| s.on);
            self.settings[band] = Some(settings);
            let coeffs = BiquadCoeffs::design(
                settings.kind,
                settings.freq,
                settings.q,
                settings.gain_db,
                self.sample_rate,
            );
            for f in &mut self.filters[band] {
                f.set_coeffs(coeffs);
                if !was_on {
                    f.reset();
                }
            }
        }
    }
}

impl NodeProcessor for Eq {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        if self.watch.changed(ctx.shared) {
            self.refresh(ctx.shared);
        }
        let input = &mut ctx.inputs[0];
        if !ctx.shared.bypassed() && !input.is_silent() {
            for (settings, filters) in self.settings.iter().zip(&mut self.filters) {
                if !settings.is_some_and(|s| s.on) {
                    continue;
                }
                for (c, filter) in filters
                    .iter_mut()
                    .enumerate()
                    .take(self.channels.min(input.channel_count()))
                {
                    filter.process(input.channel_mut(c, ctx.frames));
                }
            }
        }
        ctx.pass_through();
    }
}

struct DeEsserNode {
    inner: DeEsser,
    watch: ParamWatch,
}

impl DeEsserNode {
    fn new(channels: usize, sample_rate: f32) -> Self {
        Self {
            inner: DeEsser::new(channels, DeEsserSettings::default(), sample_rate),
            watch: ParamWatch::default(),
        }
    }
}

impl NodeProcessor for DeEsserNode {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        if self.watch.changed(ctx.shared) {
            let p = |i| ctx.shared.param(i);
            self.inner.set(DeEsserSettings {
                freq_hz: p(0),
                threshold_db: p(1),
                ratio: p(2),
                max_reduction_db: p(3),
            });
        }
        if !ctx.shared.bypassed() && !ctx.inputs[0].is_silent() {
            self.inner
                .process(&mut ctx.inputs[0].slices_mut(ctx.frames));
            ctx.shared
                .meter
                .record_reduction(self.inner.gain_reduction_db());
        }
        ctx.pass_through();
    }
    fn skips_silence(&self) -> bool {
        true
    }
}

struct CompressorNode {
    inner: Compressor,
    watch: ParamWatch,
}

impl CompressorNode {
    fn new(sample_rate: f32) -> Self {
        Self {
            inner: Compressor::new(CompressorSettings::default(), sample_rate),
            watch: ParamWatch::default(),
        }
    }
}

impl NodeProcessor for CompressorNode {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        if self.watch.changed(ctx.shared) {
            let p = |i| ctx.shared.param(i);
            self.inner.set(CompressorSettings {
                threshold_db: p(0),
                ratio: p(1),
                knee_db: p(2),
                attack_ms: p(3),
                release_ms: p(4),
                makeup_db: p(5),
            });
        }
        if !ctx.shared.bypassed() && !ctx.inputs[0].is_silent() {
            self.inner
                .process(&mut ctx.inputs[0].slices_mut(ctx.frames));
            ctx.shared
                .meter
                .record_reduction(self.inner.gain_reduction_db());
        }
        ctx.pass_through();
    }
    fn skips_silence(&self) -> bool {
        true
    }
}

struct GateNode {
    inner: Gate,
    watch: ParamWatch,
}

impl GateNode {
    fn new(sample_rate: f32) -> Self {
        Self {
            inner: Gate::new(GateSettings::default(), sample_rate),
            watch: ParamWatch::default(),
        }
    }
}

impl NodeProcessor for GateNode {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        if self.watch.changed(ctx.shared) {
            let p = |i| ctx.shared.param(i);
            self.inner.set(GateSettings {
                threshold_db: p(0),
                range_db: p(1),
                attack_ms: p(2),
                hold_ms: p(3),
                release_ms: p(4),
            });
        }
        if !ctx.shared.bypassed() && !ctx.inputs[0].is_silent() {
            self.inner
                .process(&mut ctx.inputs[0].slices_mut(ctx.frames));
            ctx.shared
                .meter
                .record_reduction(self.inner.gain_reduction_db());
        }
        ctx.pass_through();
    }
    fn skips_silence(&self) -> bool {
        true
    }
}

struct LimiterNode {
    inner: Limiter,
    sample_rate: f32,
    watch: ParamWatch,
}

impl LimiterNode {
    fn new(sample_rate: f32) -> Self {
        Self {
            inner: Limiter::new(-1.0, 50.0, sample_rate),
            sample_rate,
            watch: ParamWatch::default(),
        }
    }
}

impl NodeProcessor for LimiterNode {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        if self.watch.changed(ctx.shared) {
            self.inner
                .set(ctx.shared.param(0), ctx.shared.param(1), self.sample_rate);
        }
        if !ctx.shared.bypassed() && !ctx.inputs[0].is_silent() {
            self.inner
                .process(&mut ctx.inputs[0].slices_mut(ctx.frames));
            ctx.shared
                .meter
                .record_reduction(self.inner.gain_reduction_db());
        }
        ctx.pass_through();
    }
    fn skips_silence(&self) -> bool {
        true
    }
}

struct DenoiseNode {
    denoisers: Vec<Denoiser>,
    dry: crate::buffer::DelayLine,
    idle_frames: usize,
}

impl DenoiseNode {
    fn new(channels: usize, max_block: usize) -> Self {
        Self {
            denoisers: (0..channels).map(|_| Denoiser::new()).collect(),
            dry: crate::buffer::DelayLine::new(channels, Denoiser::LATENCY_FRAMES, max_block),
            idle_frames: usize::MAX,
        }
    }
}

impl NodeProcessor for DenoiseNode {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        let frames = ctx.frames;
        let input = &mut ctx.inputs[0];
        if input.is_silent() {
            self.idle_frames = self.idle_frames.saturating_add(frames);
            if self.idle_frames > 2 * Denoiser::LATENCY_FRAMES {
                ctx.pass_through();
                return;
            }
        } else {
            self.idle_frames = 0;
        }
        let mix = if ctx.shared.bypassed() {
            0.0
        } else {
            ctx.shared.param(0)
        };
        self.dry.scratch_mut().mix_from(input, frames, 1.0, false);
        let dry = self.dry.process_scratch(frames);
        for (c, denoiser) in self
            .denoisers
            .iter_mut()
            .enumerate()
            .take(input.channel_count())
        {
            let wet = input.channel_mut(c, frames);
            denoiser.process(wet);
            if mix < 1.0 {
                for (w, &d) in wet.iter_mut().zip(dry.channel(c, frames)) {
                    *w = *w * mix + d * (1.0 - mix);
                }
            }
        }
        ctx.pass_through();
    }

    fn latency(&self) -> usize {
        Denoiser::LATENCY_FRAMES
    }
}
