use crossbeam_channel::Sender;

use crate::daemon::{Inbound, TrayAction, TrayStatus, TrayView};

#[cfg(all(target_os = "linux", feature = "tray"))]
pub fn spawn(inbox: Sender<Inbound>) -> Option<Box<dyn TrayView>> {
    linux::spawn(inbox)
}

#[cfg(all(windows, feature = "tray"))]
pub fn spawn(inbox: Sender<Inbound>) -> Option<Box<dyn TrayView>> {
    windows::spawn(inbox)
}

#[cfg(not(any(
    all(target_os = "linux", feature = "tray"),
    all(windows, feature = "tray")
)))]
pub fn spawn(_: Sender<Inbound>) -> Option<Box<dyn TrayView>> {
    None
}

#[allow(dead_code)]
fn send(inbox: &Sender<Inbound>, action: TrayAction) {
    let _ = inbox.send(Inbound::Tray(action));
}

#[cfg(all(target_os = "linux", feature = "tray"))]
mod linux {
    use ksni::blocking::{Handle, TrayMethods};
    use ksni::menu::{CheckmarkItem, StandardItem};
    use ksni::{MenuItem, ToolTip};

    use super::*;

    struct Tray {
        status: TrayStatus,
        inbox: Sender<Inbound>,
    }

    impl ksni::Tray for Tray {
        fn id(&self) -> String {
            "tonewire".into()
        }

        fn title(&self) -> String {
            "tonewire".into()
        }

        fn icon_name(&self) -> String {
            if self.status.muted {
                "audio-volume-muted".into()
            } else {
                "audio-card".into()
            }
        }

        fn tool_tip(&self) -> ToolTip {
            ToolTip {
                title: "tonewire".into(),
                description: self.status.line.clone(),
                ..ToolTip::default()
            }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            send(&self.inbox, TrayAction::OpenEditor);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem {
                    label: "Open editor".into(),
                    activate: Box::new(|t: &mut Self| send(&t.inbox, TrayAction::OpenEditor)),
                    ..StandardItem::default()
                }
                .into(),
                CheckmarkItem {
                    label: "Mute all outputs".into(),
                    checked: self.status.muted,
                    activate: Box::new(|t: &mut Self| send(&t.inbox, TrayAction::ToggleMute)),
                    ..CheckmarkItem::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: self.status.line.clone(),
                    enabled: false,
                    ..StandardItem::default()
                }
                .into(),
                StandardItem {
                    label: "Quit".into(),
                    activate: Box::new(|t: &mut Self| send(&t.inbox, TrayAction::Quit)),
                    ..StandardItem::default()
                }
                .into(),
            ]
        }
    }

    struct View(Handle<Tray>);

    impl TrayView for View {
        fn update(&mut self, status: &TrayStatus) {
            let status = status.clone();
            self.0.update(move |t| t.status = status);
        }
    }

    pub fn spawn(inbox: Sender<Inbound>) -> Option<Box<dyn TrayView>> {
        let tray = Tray {
            status: TrayStatus::default(),
            inbox,
        };
        match tray.spawn() {
            Ok(handle) => Some(Box::new(View(handle))),
            Err(e) => {
                log::info!("no tray icon: {e}");
                None
            }
        }
    }
}

#[cfg(all(windows, feature = "tray"))]
mod windows {
    use crossbeam_channel::{Receiver, unbounded};
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, MSG, MsgWaitForMultipleObjects, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
        TranslateMessage,
    };

    use super::*;

    struct View(Sender<TrayStatus>);

    impl TrayView for View {
        fn update(&mut self, status: &TrayStatus) {
            let _ = self.0.send(status.clone());
        }
    }

    fn icon() -> Option<Icon> {
        const SIZE: u32 = 32;
        let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
        for y in 0..SIZE {
            for x in 0..SIZE {
                let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
                let inside = dx * dx + dy * dy <= 15.0 * 15.0;
                rgba.extend_from_slice(if inside {
                    &[0x3d, 0x9b, 0xe9, 0xff]
                } else {
                    &[0, 0, 0, 0]
                });
            }
        }
        Icon::from_rgba(rgba, SIZE, SIZE).ok()
    }

    pub fn spawn(inbox: Sender<Inbound>) -> Option<Box<dyn TrayView>> {
        let (tx, rx) = unbounded();
        std::thread::Builder::new()
            .name("tray".into())
            .spawn(move || {
                if let Err(e) = run(inbox, rx) {
                    log::info!("no tray icon: {e}");
                }
            })
            .ok()?;
        Some(Box::new(View(tx)))
    }

    fn run(inbox: Sender<Inbound>, updates: Receiver<TrayStatus>) -> Result<(), String> {
        let open = MenuItem::new("Open editor", true, None);
        let mute = CheckMenuItem::new("Mute all outputs", true, false, None);
        let status = MenuItem::new("Starting", false, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[
            &open,
            &mute,
            &PredefinedMenuItem::separator(),
            &status,
            &quit,
        ])
        .map_err(|e| e.to_string())?;
        let mut builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("tonewire");
        if let Some(icon) = icon() {
            builder = builder.with_icon(icon);
        }
        let tray = builder.build().map_err(|e| e.to_string())?;

        loop {
            // SAFETY: plain Win32 message pump on the thread that owns the tray window.
            unsafe {
                MsgWaitForMultipleObjects(0, std::ptr::null(), 0, 200, QS_ALLINPUT);
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let action = if event.id == *open.id() {
                    TrayAction::OpenEditor
                } else if event.id == *mute.id() {
                    TrayAction::ToggleMute
                } else if event.id == *quit.id() {
                    TrayAction::Quit
                } else {
                    continue;
                };
                send(&inbox, action);
                if action == TrayAction::Quit {
                    return Ok(());
                }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                    send(&inbox, TrayAction::OpenEditor);
                }
            }
            while let Ok(update) = updates.try_recv() {
                mute.set_checked(update.muted);
                status.set_text(&update.line);
                let _ = tray.set_tooltip(Some(format!("tonewire: {}", update.line)));
            }
        }
    }
}
