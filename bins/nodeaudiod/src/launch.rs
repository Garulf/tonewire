use std::path::PathBuf;
use std::process::Command;

fn sibling(name: &str) -> PathBuf {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&exe)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(exe))
}

pub fn editor() {
    if let Err(e) = Command::new(sibling("nodeaudio")).spawn() {
        log::error!("cannot start the editor: {e}");
    }
}

pub fn daemon_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| sibling("nodeaudiod"))
}
