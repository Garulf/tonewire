use std::collections::BTreeMap;

use tonewire_graph::{AppStream, Direction};
use wasapi::DeviceEnumerator;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

/// Running processes as `(pid, executable name)`.
pub fn processes() -> Vec<(u32, String)> {
    let mut result = Vec::new();
    // SAFETY: standard Toolhelp snapshot walk; the handle is closed below.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return result;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
        while ok {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            result.push((
                entry.th32ProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..len]),
            ));
            ok = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    result
}

/// Apps with an audio session on any active device.
pub fn scan() -> Vec<AppStream> {
    let names: BTreeMap<u32, String> = processes().into_iter().collect();
    let mut seen = BTreeMap::new();
    let Ok(enumerator) = DeviceEnumerator::new() else {
        return Vec::new();
    };
    for (direction, flow) in [
        (Direction::Playback, wasapi::Direction::Render),
        (Direction::Capture, wasapi::Direction::Capture),
    ] {
        let Ok(collection) = enumerator.get_device_collection(&flow) else {
            continue;
        };
        let count = collection.get_nbr_devices().unwrap_or(0);
        for index in 0..count {
            let Some(sessions) = collection
                .get_device_at_index(index)
                .and_then(|d| d.get_iaudiosessionmanager())
                .and_then(|m| m.get_audiosessionenumerator())
                .ok()
            else {
                continue;
            };
            for i in 0..sessions.get_count().unwrap_or(0) {
                let Ok(pid) = sessions.get_session(i).and_then(|s| s.get_process_id()) else {
                    continue;
                };
                if pid == 0 {
                    continue;
                }
                if let Some(binary) = names.get(&pid) {
                    seen.entry((pid, direction == Direction::Capture))
                        .or_insert(AppStream {
                            id: u64::from(pid),
                            binary: binary.clone(),
                            name: binary.trim_end_matches(".exe").to_owned(),
                            direction,
                        });
                }
            }
        }
    }
    seen.into_values().collect()
}
