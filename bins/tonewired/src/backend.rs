use tonewire_graph::{ExternalFactory, NvAfxEffect};
use tonewire_io::{NullPlatform, Platform, StreamConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    Auto,
    Null,
}

impl std::str::FromStr for BackendChoice {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Self::Auto),
            "null" => Ok(Self::Null),
            other => Err(format!("unknown backend `{other}` (expected auto or null)")),
        }
    }
}

pub fn create(choice: &BackendChoice, config: StreamConfig) -> Result<Box<dyn Platform>, String> {
    match choice {
        BackendChoice::Null => Ok(Box::new(NullPlatform::start(config).0)),
        BackendChoice::Auto => native(config),
    }
}

#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn native(config: StreamConfig) -> Result<Box<dyn Platform>, String> {
    tonewire_io::pipewire::PipeWirePlatform::connect(config)
        .map(|p| Box::new(p) as Box<dyn Platform>)
        .map_err(|e| format!("cannot connect to PipeWire: {e}"))
}

#[cfg(windows)]
fn native(config: StreamConfig) -> Result<Box<dyn Platform>, String> {
    tonewire_io::wasapi::WasapiPlatform::start(config)
        .map(|p| Box::new(p) as Box<dyn Platform>)
        .map_err(|e| format!("cannot start WASAPI: {e}"))
}

#[cfg(not(any(all(target_os = "linux", feature = "pipewire"), windows)))]
fn native(_: StreamConfig) -> Result<Box<dyn Platform>, String> {
    Err("this build has no native audio backend; use --backend null".into())
}

pub fn nvafx_unavailable(
    external: Option<&dyn ExternalFactory>,
    effect: &NvAfxEffect,
) -> Option<String> {
    match external {
        None => Some("NVIDIA Audio Effects SDK not found".into()),
        Some(factory) => factory.unavailable(&tonewire_graph::NodeKind::NvAfx {
            effect: effect.clone(),
        }),
    }
}
