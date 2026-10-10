use std::io;

use crate::launch::daemon_path;

#[cfg(windows)]
pub fn set(enabled: bool) -> io::Result<()> {
    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    let key = windows_registry::CURRENT_USER
        .create(RUN)
        .map_err(io::Error::other)?;
    if enabled {
        let command = format!("\"{}\"", daemon_path().display());
        key.set_string("nodeaudiod", &command)
            .map_err(io::Error::other)
    } else {
        match key.remove_value("nodeaudiod") {
            Ok(()) => Ok(()),
            Err(e) if e.code() == windows_result::HRESULT::from_win32(2) => Ok(()),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}

#[cfg(not(windows))]
pub fn set(enabled: bool) -> io::Result<()> {
    let dir = directories::BaseDirs::new()
        .map(|d| d.config_dir().join("autostart"))
        .ok_or_else(|| io::Error::other("no home directory"))?;
    let file = dir.join("nodeaudiod.desktop");
    if enabled {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&file, desktop_entry())
    } else {
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(not(windows))]
fn desktop_entry() -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=nodeaudio daemon\nExec=\"{}\"\nX-GNOME-Autostart-enabled=true\nNoDisplay=true\n",
        daemon_path().display()
    )
}
