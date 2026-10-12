//! StatusNotifier tray (ksni): the Linux analogue of upstream's menu-bar
//! item: Show/Hide, Take everything down, Open screenshots folder,
//! Sounds ✓, Launch at login ✓, Quit. Callbacks only send [`Msg`]; the
//! GTK tick applies everything on the main thread, so no `Rc` crosses
//! threads (the tray service owns its threads).

use std::sync::{mpsc, Arc, Mutex};

use ksni::{
    menu::{CheckmarkItem, Disposition, MenuItem, StandardItem},
    Tray,
};

/// Tray pixels, embedded at build time: the host tray looks up theme names
/// on its own side, so a theme name would break under AppImage and unpacked
/// runs. Pixmaps travel over the bus and work everywhere.
const TRAY_PNG: &[u8] =
    include_bytes!("../../../packaging/icons/hicolor/24x24/apps/io.github.mansher.Mansher.png");

fn tray_pixmap() -> Vec<ksni::Icon> {
    let Ok(img) = image::load_from_memory(TRAY_PNG) else {
        return Vec::new();
    };
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let mut argb = Vec::with_capacity((w * h * 4) as usize);
    for p in rgba.pixels() {
        argb.extend_from_slice(&[p[3], p[0], p[1], p[2]]);
    }
    vec![ksni::Icon {
        width: w as i32,
        height: h as i32,
        data: argb,
    }]
}

use crate::{settings::autostart_desktop, Msg};

pub struct MansherTray {
    tx: mpsc::Sender<Msg>,
    sound_on: Arc<Mutex<bool>>,
}

impl MansherTray {
    pub fn new(tx: mpsc::Sender<Msg>, sound_on: Arc<Mutex<bool>>) -> Self {
        Self { tx, sound_on }
    }

    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    fn standard(&self, label: &str, msg: Msg) -> MenuItem<Self> {
        let tx = self.tx.clone();
        MenuItem::Standard(StandardItem {
            label: label.into(),
            enabled: true,
            visible: true,
            icon_name: String::new(),
            icon_data: Vec::new(),
            shortcut: Vec::new(),
            disposition: Disposition::Normal,
            activate: Box::new(move |_| {
                let _ = tx.send(msg.clone());
            }),
        })
    }
}

impl Tray for MansherTray {
    fn id(&self) -> String {
        "io.github.mansher.Mansher".into()
    }

    fn title(&self) -> String {
        "Mansher".into()
    }

    fn icon_name(&self) -> String {
        // Empty on purpose: with no theme name the host takes icon_pixmap,
        // which carries our own pixels instead of a stock lookup.
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        tray_pixmap()
    }

    /// Left-click toggles the line, like upstream's menu-bar toggle.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Msg::UiToggle);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let sound = *self.sound_on.lock().unwrap_or_else(|e| e.into_inner());
        let tx = self.tx.clone();
        vec![
            self.standard("Show / Hide line", Msg::UiToggle),
            self.standard("Take everything down", Msg::UiClearAll),
            MenuItem::Separator,
            self.standard("Open screenshots folder", Msg::UiOpenFolder),
            MenuItem::Checkmark(CheckmarkItem {
                label: "Sounds".into(),
                enabled: true,
                visible: true,
                checked: sound,
                icon_name: String::new(),
                icon_data: Vec::new(),
                shortcut: Vec::new(),
                disposition: Disposition::Normal,
                activate: Box::new(move |_| {
                    let _ = tx.send(Msg::UiSoundsToggle);
                }),
            }),
            MenuItem::Checkmark(CheckmarkItem {
                label: "Launch at login".into(),
                enabled: true,
                visible: true,
                checked: autostart_desktop().exists(),
                icon_name: String::new(),
                icon_data: Vec::new(),
                shortcut: Vec::new(),
                disposition: Disposition::Normal,
                activate: Box::new({
                    let tx = self.tx.clone();
                    move |_| {
                        let _ = tx.send(Msg::UiAutostartToggle);
                    }
                }),
            }),
            MenuItem::Separator,
            self.standard("Preferences", Msg::UiPreferences),
            self.standard("Quit Mansher", Msg::UiQuit),
        ]
    }
}

/// Run the tray service on its own thread. A StatusNotifierWatcher is
/// required (DankShell provides one here); without it this logs and the
/// app keeps working via hotkey + hover + peek.
pub fn start(tx: mpsc::Sender<Msg>, sound_on: Arc<Mutex<bool>>) {
    std::thread::spawn(move || {
        use ksni::blocking::TrayMethods;
        match MansherTray::new(tx, sound_on).spawn() {
            Ok(_handle) => {
                // Service lives on its own threads; park ours so nothing
                // is dropped out from under it.
                std::thread::park();
            }
            Err(e) => eprintln!("mansher: tray unavailable: {e:#}"),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_pixmap_decodes_embedded_png_to_argb() {
        let icons = tray_pixmap();
        assert_eq!(icons.len(), 1);
        assert_eq!((icons[0].width, icons[0].height), (24, 24));
        assert_eq!(icons[0].data.len(), 24 * 24 * 4);
        // ARGB order: alpha first. The squircle corners stay transparent
        // while the mark itself is opaque.
        assert!(icons[0].data.iter().step_by(4).any(|&a| a == 255));
        assert!(icons[0].data.iter().step_by(4).any(|&a| a == 0));
    }
}
