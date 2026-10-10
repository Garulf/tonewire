use crate::buffer::AudioBuf;
use crate::model::NodeId;
use crate::shared::NodeShared;

/// The engine's view of device and virtual endpoints, supplied by the I/O
/// backend on the audio thread. Implementations must not block or allocate.
pub trait EndpointIo {
    /// Fills `dest` with captured audio. Returns `false` when the endpoint has
    /// nothing to deliver; `dest` is then left untouched.
    fn read(&mut self, node: NodeId, dest: &mut AudioBuf, frames: usize) -> bool;
    fn write(&mut self, node: NodeId, src: &AudioBuf, frames: usize);
}

/// An `EndpointIo` with no endpoints, for offline processing and tests.
#[derive(Debug, Default)]
pub struct NoEndpoints;

impl EndpointIo for NoEndpoints {
    fn read(&mut self, _: NodeId, _: &mut AudioBuf, _: usize) -> bool {
        false
    }
    fn write(&mut self, _: NodeId, _: &AudioBuf, _: usize) {}
}

pub struct ProcessCtx<'a> {
    pub node: NodeId,
    pub frames: usize,
    pub inputs: &'a mut [AudioBuf],
    pub outputs: &'a mut [AudioBuf],
    pub shared: &'a NodeShared,
    pub io: &'a mut dyn EndpointIo,
    /// Fade applied by sink nodes while the engine swaps plans.
    pub fade: (f32, f32),
}

impl ProcessCtx<'_> {
    /// Moves input 0 to output 0 without copying; used by in-place effects.
    pub fn pass_through(&mut self) {
        if let (Some(input), Some(output)) = (self.inputs.first_mut(), self.outputs.first_mut()) {
            if input.channel_count() == output.channel_count() {
                std::mem::swap(input, output);
            } else {
                output.mix_from(input, self.frames, 1.0, false);
            }
        }
    }
}

pub trait NodeProcessor: Send {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>);

    /// Frames of delay this node adds.
    fn latency(&self) -> usize {
        0
    }

    /// True when silent input is guaranteed to produce silent output, so the
    /// engine may skip the call entirely.
    fn skips_silence(&self) -> bool {
        false
    }
}
