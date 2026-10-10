#![deny(unsafe_op_in_unsafe_fn)]

mod ffi;
mod node;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use nodeaudio_graph::{ExternalFactory, NodeKind, NodeProcessor, NvAfxEffect};

pub use node::{FrameEffect, WorkerNode};

const SAMPLE_RATE: u32 = 48_000;

fn candidates(configured: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = configured.map(Path::to_path_buf).into_iter().collect();
    for var in ["NVAFX_SDK_DIR", "NVAFX_SDK_ROOT"] {
        if let Some(dir) = std::env::var_os(var) {
            dirs.push(dir.into());
        }
    }
    if cfg!(windows) {
        dirs.push(r"C:\Program Files\NVIDIA Corporation\NVIDIA Audio Effects SDK".into());
    } else {
        dirs.push("/usr/local/AudioFX".into());
    }
    let mut files = Vec::new();
    for dir in dirs {
        files.push(dir.join(ffi::LIBRARY));
        files.push(dir.join("bin").join(ffi::LIBRARY));
        files.push(dir.join("nvafx").join("lib").join(ffi::LIBRARY));
        files.push(dir.join("lib").join(ffi::LIBRARY));
    }
    files
}

fn selector(effect: &NvAfxEffect, available: &[String]) -> Option<String> {
    let exact = match effect {
        NvAfxEffect::Denoiser => "denoiser",
        NvAfxEffect::Dereverb => "dereverb",
        NvAfxEffect::DereverbDenoiser => "dereverb_denoiser",
        NvAfxEffect::StudioVoice => "studio_voice",
        NvAfxEffect::SpeakerFocus => "speaker_focus",
    };
    available
        .iter()
        .find(|e| *e == exact)
        .or_else(|| {
            available
                .iter()
                .find(|e| e.starts_with(exact) && !e.contains("16k"))
        })
        .cloned()
}

/// Finds a model file for `selector` in the SDK tree, if the SDK ships them
/// as separate files.
fn model_for(root: &Path, selector: &str) -> Option<PathBuf> {
    let mut stack = vec![root.parent().unwrap_or(root).to_path_buf()];
    let mut depth = 0;
    while let Some(dir) = stack.pop() {
        depth += 1;
        if depth > 64 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if path.is_dir() {
                stack.push(path);
            } else if name.starts_with(selector)
                && name.contains("48k")
                && name.ends_with(".trtpkg")
            {
                return Some(path);
            }
        }
    }
    None
}

struct Loaded {
    api: ffi::Api,
    effects: Vec<String>,
}

/// Loads the SDK the first time an NVIDIA node is created, so the daemon
/// stays small when the graph has none.
pub struct NvAfxFactory {
    library: PathBuf,
    loaded: OnceLock<Result<&'static Loaded, String>>,
}

impl NvAfxFactory {
    fn loaded(&self) -> Result<&'static Loaded, String> {
        self.loaded
            .get_or_init(|| {
                let api = ffi::Api::load(&self.library)?;
                let effects = api.effects();
                log::info!(
                    "loaded NVIDIA Audio Effects SDK; effects: {}",
                    effects.join(", ")
                );
                Ok(&*Box::leak(Box::new(Loaded { api, effects })))
            })
            .clone()
    }
}

struct SdkEffect {
    effect: ffi::Effect<'static>,
    frame: usize,
}

impl FrameEffect for SdkEffect {
    fn frame_len(&self) -> usize {
        self.frame
    }

    fn set_intensity(&mut self, intensity: f32) {
        if let Err(e) = self.effect.set_float("intensity_ratio", intensity) {
            log::debug!("intensity not supported: {e}");
        }
    }

    fn run(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), String> {
        self.effect.run(input, output)
    }
}

impl ExternalFactory for NvAfxFactory {
    fn unavailable(&self, kind: &NodeKind) -> Option<String> {
        let NodeKind::NvAfx { effect } = kind else {
            return Some("not an NVIDIA effect".into());
        };
        match self.loaded.get() {
            Some(Ok(loaded)) if selector(effect, &loaded.effects).is_none() => Some(format!(
                "the installed SDK has no {} effect",
                effect.label()
            )),
            Some(Err(e)) => Some(e.clone()),
            _ => None,
        }
    }

    fn create(
        &self,
        kind: &NodeKind,
        _channels: usize,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<Box<dyn NodeProcessor>, String> {
        let NodeKind::NvAfx { effect } = kind else {
            return Err("not an NVIDIA effect".into());
        };
        if sample_rate as u32 != SAMPLE_RATE {
            return Err("NVIDIA effects need the engine at 48 kHz".into());
        }
        let loaded = self.loaded()?;
        let selector = selector(effect, &loaded.effects)
            .ok_or_else(|| format!("the installed SDK has no {} effect", effect.label()))?;
        let mut sdk = ffi::Effect::create(&loaded.api, &selector)?;
        if let Some(model) = model_for(loaded.api.root(), &selector) {
            sdk.set_string("model_path", &model.to_string_lossy())?;
        }
        let _ = sdk.set_u32("input_sample_rate", SAMPLE_RATE);
        let _ = sdk.set_u32("output_sample_rate", SAMPLE_RATE);
        let _ = sdk.set_u32("use_default_gpu", 1);
        sdk.load()?;
        let frame = sdk
            .get_u32("num_input_samples_per_frame")
            .or_else(|_| sdk.get_u32("num_samples_per_frame"))
            .unwrap_or(480) as usize;
        let node = WorkerNode::spawn(Box::new(SdkEffect { effect: sdk, frame }), max_block)?;
        Ok(Box::new(node))
    }
}

/// The NVIDIA effects factory, or `None` when no SDK library is installed.
/// Nothing is loaded until an NVIDIA node is created.
pub fn factory(sdk_dir: Option<&Path>) -> Option<Arc<dyn ExternalFactory>> {
    let library = candidates(sdk_dir).into_iter().find(|p| p.is_file())?;
    log::info!("found NVIDIA Audio Effects SDK at {}", library.display());
    Some(Arc::new(NvAfxFactory {
        library,
        loaded: OnceLock::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_sdk_yields_no_factory() {
        let dir = std::env::temp_dir().join("nodeaudio-no-sdk-here");
        let installed = candidates(Some(&dir)).iter().any(|p| p.is_file());
        assert_eq!(factory(Some(&dir)).is_some(), installed);
    }

    #[test]
    fn selectors_prefer_exact_48k_names() {
        let list: Vec<String> = [
            "denoiser16k_superres16kto48k",
            "denoiser",
            "studio_voice_high_quality",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            selector(&NvAfxEffect::Denoiser, &list).as_deref(),
            Some("denoiser")
        );
        assert_eq!(
            selector(&NvAfxEffect::StudioVoice, &list).as_deref(),
            Some("studio_voice_high_quality")
        );
        assert_eq!(selector(&NvAfxEffect::SpeakerFocus, &list), None);
    }

    #[test]
    fn loading_a_bogus_library_reports_an_error() {
        let path = std::env::temp_dir().join(format!("bogus-{}", ffi::LIBRARY));
        std::fs::write(&path, b"not a library").unwrap();
        let f = NvAfxFactory {
            library: path,
            loaded: OnceLock::new(),
        };
        let kind = NodeKind::NvAfx {
            effect: NvAfxEffect::Denoiser,
        };
        assert!(f.create(&kind, 1, 48_000.0, 128).is_err());
        assert!(f.unavailable(&kind).is_some());
    }
}
