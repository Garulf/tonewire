use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::catalog;
use crate::model::NodeSpec;

#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn store(&self, value: f32) {
        self.0.store(value.to_bits(), Ordering::Relaxed);
    }

    pub fn swap(&self, value: f32) -> f32 {
        f32::from_bits(self.0.swap(value.to_bits(), Ordering::Relaxed))
    }

    /// Only valid for non-negative values, whose bit patterns sort like the floats.
    pub fn fetch_max_positive(&self, value: f32) {
        self.0
            .fetch_max(value.max(0.0).to_bits(), Ordering::Relaxed);
    }
}

#[derive(Debug, Default)]
pub struct MeterSlot {
    peak: AtomicF32,
    rms: AtomicF32,
    reduction_db: AtomicF32,
}

impl MeterSlot {
    pub fn record(&self, peak: f32, rms: f32) {
        self.peak.fetch_max_positive(peak);
        self.rms.store(rms);
    }

    pub fn record_reduction(&self, db: f32) {
        self.reduction_db.store(db);
    }

    /// Peak since the last call, latest RMS and latest gain reduction.
    pub fn take(&self) -> (f32, f32, f32) {
        (
            self.peak.swap(0.0),
            self.rms.load(),
            self.reduction_db.load(),
        )
    }
}

/// Values shared between the control thread and a node's processor. The
/// processor polls `version` once per block and re-reads params when it moves.
#[derive(Debug)]
pub struct NodeShared {
    params: Box<[AtomicF32]>,
    version: AtomicU32,
    bypass: AtomicBool,
    pub meter: MeterSlot,
}

impl NodeShared {
    pub fn new(spec: &NodeSpec) -> Self {
        let info = catalog::describe(&spec.kind);
        let shared = Self {
            params: info
                .params
                .iter()
                .map(|p| AtomicF32::new(p.default))
                .collect(),
            version: AtomicU32::new(0),
            bypass: AtomicBool::new(false),
            meter: MeterSlot::default(),
        };
        shared.load_from(spec);
        shared
    }

    pub fn param_count(&self) -> usize {
        self.params.len()
    }

    pub fn param(&self, index: usize) -> f32 {
        self.params.get(index).map_or(0.0, AtomicF32::load)
    }

    pub fn version(&self) -> u32 {
        self.version.load(Ordering::Acquire)
    }

    pub fn bypassed(&self) -> bool {
        self.bypass.load(Ordering::Relaxed)
    }

    pub fn set_param(&self, index: usize, value: f32) {
        if let Some(slot) = self.params.get(index)
            && slot.load() != value
        {
            slot.store(value);
            self.version.fetch_add(1, Ordering::Release);
        }
    }

    pub fn load_from(&self, spec: &NodeSpec) {
        let info = catalog::describe(&spec.kind);
        for (i, p) in info.params.iter().enumerate() {
            self.set_param(i, spec.param(&p.key));
        }
        self.bypass.store(spec.bypass, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NodeKind;

    #[test]
    fn version_moves_only_on_change() {
        let shared = NodeShared::new(&NodeSpec::new(NodeKind::Gain));
        let v = shared.version();
        shared.set_param(0, 0.0);
        assert_eq!(shared.version(), v);
        shared.set_param(0, -6.0);
        assert_eq!(shared.version(), v + 1);
        assert_eq!(shared.param(0), -6.0);
    }

    #[test]
    fn meter_peak_holds_until_taken() {
        let m = MeterSlot::default();
        m.record(0.5, 0.1);
        m.record(0.2, 0.05);
        assert_eq!(m.take().0, 0.5);
        assert_eq!(m.take().0, 0.0);
    }
}
