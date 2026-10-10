use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tonewire_graph::Graph;
use tonewire_graph::format::{from_toml, to_toml};
use tonewire_proto::EngineSettings;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub sample_rate: u32,
    pub block_frames: u32,
    /// Folder of the NVIDIA Audio Effects SDK, if not in a standard location.
    pub nvafx_sdk: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        let engine = EngineSettings::default();
        Self {
            sample_rate: engine.sample_rate,
            block_frames: engine.block_frames,
            nvafx_sdk: None,
        }
    }
}

impl Settings {
    pub fn engine(&self) -> EngineSettings {
        EngineSettings {
            sample_rate: self.sample_rate,
            block_frames: self.block_frames,
        }
    }

    pub fn set_engine(&mut self, engine: EngineSettings) {
        self.sample_rate = engine.sample_rate.clamp(8_000, 192_000);
        self.block_frames = engine.block_frames.clamp(16, 1024);
    }
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn graph_path(&self) -> PathBuf {
        self.dir.join("graph.toml")
    }

    fn settings_path(&self) -> PathBuf {
        self.dir.join("settings.toml")
    }

    /// Loads the saved graph. A file that cannot be read as a graph is moved
    /// aside to `graph.toml.bad` so the daemon still starts.
    pub fn load_graph(&self) -> Graph {
        let path = self.graph_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Graph::default(),
            Err(e) => {
                log::error!("cannot read {}: {e}", path.display());
                return Graph::default();
            }
        };
        match from_toml(&text) {
            Ok(graph) => graph,
            Err(e) => {
                let bad = path.with_extension("toml.bad");
                log::error!("{}: {e}; moved to {}", path.display(), bad.display());
                if let Err(e) = std::fs::rename(&path, &bad) {
                    log::error!("cannot move bad graph aside: {e}");
                }
                Graph::default()
            }
        }
    }

    pub fn save_graph(&self, graph: &Graph) -> io::Result<()> {
        write_atomic(&self.graph_path(), &to_toml(graph))
    }

    pub fn load_settings(&self) -> Settings {
        std::fs::read_to_string(self.settings_path())
            .ok()
            .and_then(|text| match toml::from_str(&text) {
                Ok(settings) => Some(settings),
                Err(e) => {
                    log::error!("settings.toml: {e}; using defaults");
                    None
                }
            })
            .unwrap_or_default()
    }

    pub fn save_settings(&self, settings: &Settings) -> io::Result<()> {
        let text = toml::to_string_pretty(settings).map_err(io::Error::other)?;
        write_atomic(&self.settings_path(), &text)
    }
}

fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tonewire_graph::{NodeKind, NodeSpec};

    fn temp_store(tag: &str) -> Store {
        let dir =
            std::env::temp_dir().join(format!("tonewired-persist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::new(dir).unwrap()
    }

    #[test]
    fn graph_round_trips_through_disk() {
        let store = temp_store("roundtrip");
        let mut g = Graph::default();
        g.add_node(NodeSpec::new(NodeKind::Compressor));
        store.save_graph(&g).unwrap();
        assert_eq!(store.load_graph(), g);
    }

    #[test]
    fn corrupt_graph_is_moved_aside() {
        let store = temp_store("corrupt");
        std::fs::write(store.graph_path(), "this is [not a graph").unwrap();
        assert_eq!(store.load_graph(), Graph::default());
        assert!(store.dir().join("graph.toml.bad").exists());
        assert!(!store.graph_path().exists());
    }

    #[test]
    fn engine_settings_are_clamped() {
        let mut s = Settings::default();
        s.set_engine(EngineSettings {
            sample_rate: 1,
            block_frames: 1_000_000,
        });
        assert_eq!((s.sample_rate, s.block_frames), (8_000, 1024));
    }
}
