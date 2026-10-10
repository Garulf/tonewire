use serde::{Deserialize, Serialize};

use crate::model::{NodeKind, NvAfxEffect};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Category {
    Io,
    Routing,
    Tone,
    Dynamics,
    Ai,
    Analysis,
}

impl Category {
    pub const ALL: [Self; 6] = [
        Self::Io,
        Self::Routing,
        Self::Tone,
        Self::Dynamics,
        Self::Ai,
        Self::Analysis,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Io => "Inputs & outputs",
            Self::Routing => "Routing",
            Self::Tone => "Tone",
            Self::Dynamics => "Dynamics",
            Self::Ai => "AI",
            Self::Analysis => "Analysis",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Unit {
    None,
    Db,
    Hz,
    Ms,
    Ratio,
    Percent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ParamKind {
    Continuous { log: bool },
    Toggle,
    Choice(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamInfo {
    pub key: String,
    pub label: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub unit: Unit,
    pub kind: ParamKind,
}

impl ParamInfo {
    fn new(
        key: impl Into<String>,
        label: impl Into<String>,
        range: (f32, f32),
        default: f32,
        unit: Unit,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            min: range.0,
            max: range.1,
            default,
            unit,
            kind: ParamKind::Continuous {
                log: matches!(unit, Unit::Hz),
            },
        }
    }

    fn toggle(key: impl Into<String>, label: impl Into<String>, default: bool) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            min: 0.0,
            max: 1.0,
            default: f32::from(u8::from(default)),
            unit: Unit::None,
            kind: ParamKind::Toggle,
        }
    }

    fn choice(
        key: impl Into<String>,
        label: impl Into<String>,
        options: &[&str],
        default: usize,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            min: 0.0,
            max: (options.len() - 1) as f32,
            default: default as f32,
            unit: Unit::None,
            kind: ParamKind::Choice(options.iter().map(|s| (*s).to_owned()).collect()),
        }
    }

    pub fn clamp(&self, value: f32) -> f32 {
        let value = if value.is_finite() {
            value
        } else {
            self.default
        };
        let value = value.clamp(self.min, self.max);
        match self.kind {
            ParamKind::Continuous { .. } => value,
            ParamKind::Toggle | ParamKind::Choice(_) => value.round(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KindInfo {
    pub label: String,
    pub category: Category,
    pub inputs: u8,
    pub outputs: u8,
    pub default_channels: u8,
    pub params: Vec<ParamInfo>,
}

impl KindInfo {
    pub fn param(&self, key: &str) -> Option<&ParamInfo> {
        self.params.iter().find(|p| p.key == key)
    }

    pub fn param_index(&self, key: &str) -> Option<usize> {
        self.params.iter().position(|p| p.key == key)
    }
}

pub const EQ_BANDS: usize = 5;
pub const EQ_TYPES: [&str; 7] = [
    "Low shelf",
    "Peak",
    "High shelf",
    "Low pass",
    "High pass",
    "Notch",
    "Band pass",
];
const EQ_DEFAULTS: [(usize, f32, bool); EQ_BANDS] = [
    (0, 100.0, true),
    (1, 400.0, true),
    (1, 2_500.0, true),
    (2, 8_000.0, true),
    (4, 30.0, false),
];

fn level_params() -> Vec<ParamInfo> {
    vec![
        ParamInfo::new("gain_db", "Gain", (-60.0, 24.0), 0.0, Unit::Db),
        ParamInfo::toggle("mute", "Mute", false),
    ]
}

fn eq_params() -> Vec<ParamInfo> {
    let mut params = Vec::with_capacity(EQ_BANDS * 5);
    for (i, (kind, freq, on)) in EQ_DEFAULTS.iter().enumerate() {
        let n = i + 1;
        params.push(ParamInfo::toggle(
            format!("band{n}_on"),
            format!("Band {n}"),
            *on,
        ));
        params.push(ParamInfo::choice(
            format!("band{n}_type"),
            format!("Band {n} type"),
            &EQ_TYPES,
            *kind,
        ));
        params.push(ParamInfo::new(
            format!("band{n}_freq"),
            format!("Band {n} freq"),
            (20.0, 20_000.0),
            *freq,
            Unit::Hz,
        ));
        params.push(ParamInfo::new(
            format!("band{n}_gain_db"),
            format!("Band {n} gain"),
            (-24.0, 24.0),
            0.0,
            Unit::Db,
        ));
        params.push(ParamInfo::new(
            format!("band{n}_q"),
            format!("Band {n} Q"),
            (0.1, 10.0),
            0.707,
            Unit::Ratio,
        ));
    }
    params
}

pub fn describe(kind: &NodeKind) -> KindInfo {
    let info = |label: &str, category, inputs, outputs, default_channels, params| KindInfo {
        label: label.to_owned(),
        category,
        inputs,
        outputs,
        default_channels,
        params,
    };
    match kind {
        NodeKind::DeviceInput => info("Device input", Category::Io, 0, 1, 2, level_params()),
        NodeKind::DeviceOutput => info("Device output", Category::Io, 1, 0, 2, level_params()),
        NodeKind::VirtualSpeaker => info("Virtual speaker", Category::Io, 0, 1, 2, level_params()),
        NodeKind::VirtualMic => info("Virtual mic", Category::Io, 1, 0, 2, level_params()),
        NodeKind::AppSource => info("App source", Category::Io, 0, 1, 2, level_params()),
        NodeKind::Mixer { inputs } => {
            let mut params: Vec<ParamInfo> = (1..=*inputs)
                .map(|i| {
                    ParamInfo::new(
                        format!("in{i}_gain_db"),
                        format!("Input {i}"),
                        (-60.0, 12.0),
                        0.0,
                        Unit::Db,
                    )
                })
                .collect();
            params.push(ParamInfo::new(
                "gain_db",
                "Output",
                (-60.0, 12.0),
                0.0,
                Unit::Db,
            ));
            info("Mixer", Category::Routing, *inputs, 1, 2, params)
        }
        NodeKind::Splitter { outputs } => {
            let params = (1..=*outputs)
                .map(|i| {
                    ParamInfo::new(
                        format!("out{i}_gain_db"),
                        format!("Output {i}"),
                        (-60.0, 12.0),
                        0.0,
                        Unit::Db,
                    )
                })
                .collect();
            info("Splitter", Category::Routing, 1, *outputs, 2, params)
        }
        NodeKind::Gain => info("Gain", Category::Routing, 1, 1, 2, level_params()),
        NodeKind::Pan => info(
            "Pan",
            Category::Routing,
            1,
            1,
            2,
            vec![ParamInfo::new("pan", "Pan", (-1.0, 1.0), 0.0, Unit::None)],
        ),
        NodeKind::Eq => info("Equalizer", Category::Tone, 1, 1, 2, eq_params()),
        NodeKind::DeEsser => info(
            "De-esser",
            Category::Dynamics,
            1,
            1,
            2,
            vec![
                ParamInfo::new("freq", "Frequency", (2_000.0, 12_000.0), 6_000.0, Unit::Hz),
                ParamInfo::new("threshold_db", "Threshold", (-60.0, 0.0), -30.0, Unit::Db),
                ParamInfo::new("ratio", "Ratio", (1.0, 20.0), 4.0, Unit::Ratio),
                ParamInfo::new(
                    "max_reduction_db",
                    "Max reduction",
                    (-24.0, 0.0),
                    -12.0,
                    Unit::Db,
                ),
            ],
        ),
        NodeKind::Compressor => info(
            "Compressor",
            Category::Dynamics,
            1,
            1,
            2,
            vec![
                ParamInfo::new("threshold_db", "Threshold", (-60.0, 0.0), -18.0, Unit::Db),
                ParamInfo::new("ratio", "Ratio", (1.0, 20.0), 3.0, Unit::Ratio),
                ParamInfo::new("knee_db", "Knee", (0.0, 24.0), 6.0, Unit::Db),
                ParamInfo::new("attack_ms", "Attack", (0.1, 200.0), 10.0, Unit::Ms),
                ParamInfo::new("release_ms", "Release", (5.0, 2_000.0), 120.0, Unit::Ms),
                ParamInfo::new("makeup_db", "Makeup", (0.0, 24.0), 0.0, Unit::Db),
            ],
        ),
        NodeKind::Gate => info(
            "Noise gate",
            Category::Dynamics,
            1,
            1,
            2,
            vec![
                ParamInfo::new("threshold_db", "Threshold", (-90.0, 0.0), -50.0, Unit::Db),
                ParamInfo::new("range_db", "Range", (-90.0, 0.0), -80.0, Unit::Db),
                ParamInfo::new("attack_ms", "Attack", (0.1, 100.0), 1.0, Unit::Ms),
                ParamInfo::new("hold_ms", "Hold", (0.0, 1_000.0), 50.0, Unit::Ms),
                ParamInfo::new("release_ms", "Release", (5.0, 2_000.0), 150.0, Unit::Ms),
            ],
        ),
        NodeKind::Limiter => info(
            "Limiter",
            Category::Dynamics,
            1,
            1,
            2,
            vec![
                ParamInfo::new("ceiling_db", "Ceiling", (-24.0, 0.0), -1.0, Unit::Db),
                ParamInfo::new("release_ms", "Release", (1.0, 1_000.0), 50.0, Unit::Ms),
            ],
        ),
        NodeKind::Denoise => info(
            "Noise suppression (RNNoise)",
            Category::Ai,
            1,
            1,
            1,
            vec![ParamInfo::new(
                "mix",
                "Amount",
                (0.0, 1.0),
                1.0,
                Unit::Percent,
            )],
        ),
        NodeKind::NvAfx { effect } => info(
            effect.label(),
            Category::Ai,
            1,
            1,
            1,
            vec![ParamInfo::new(
                "intensity",
                "Intensity",
                (0.0, 1.0),
                1.0,
                Unit::Percent,
            )],
        ),
        NodeKind::Meter => info("Meter", Category::Analysis, 1, 1, 2, Vec::new()),
    }
}

/// The node kinds offered in the editor's add menu.
pub fn menu() -> Vec<NodeKind> {
    let mut kinds = vec![
        NodeKind::DeviceInput,
        NodeKind::DeviceOutput,
        NodeKind::VirtualSpeaker,
        NodeKind::VirtualMic,
        NodeKind::AppSource,
        NodeKind::Mixer { inputs: 4 },
        NodeKind::Splitter { outputs: 2 },
        NodeKind::Gain,
        NodeKind::Pan,
        NodeKind::Eq,
        NodeKind::DeEsser,
        NodeKind::Compressor,
        NodeKind::Gate,
        NodeKind::Limiter,
        NodeKind::Denoise,
    ];
    kinds.extend(
        NvAfxEffect::ALL
            .into_iter()
            .map(|effect| NodeKind::NvAfx { effect }),
    );
    kinds.push(NodeKind::Meter);
    kinds
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_menu_kind_has_unique_param_keys_and_sane_defaults() {
        for kind in menu() {
            let info = describe(&kind);
            let mut keys: Vec<_> = info.params.iter().map(|p| &p.key).collect();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), info.params.len(), "{kind:?}");
            for p in &info.params {
                assert!(
                    p.min <= p.default && p.default <= p.max,
                    "{kind:?} {}",
                    p.key
                );
            }
        }
    }

    #[test]
    fn clamp_rounds_discrete_params_and_rejects_nan() {
        let info = describe(&NodeKind::Eq);
        let kind = info.param("band1_type").unwrap();
        assert_eq!(kind.clamp(2.4), 2.0);
        assert_eq!(kind.clamp(f32::NAN), kind.default);
    }
}
