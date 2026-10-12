//! StatusNotifier tray (ksni): the Linux analogue of upstream's menu-bar
//! item — Show/Hide, Take everything down, Open screenshots folder,
//! Sounds ✓, Launch at login ✓, Quit. Callbacks only send [`Msg`]; the
//! GTK tick applies everything on the main thread, so no `Rc` crosses
//! threads (the tray service owns its threads).

use std::sync::{mpsc, Arc, Mutex};

use ksni::{
    menu::{CheckmarkItem, Disposition, MenuItem, StandardItem},
    Tray,
};

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
        // Freedesktop standard name; upstream ships a custom t-shirt icon
        // we deliberately do not copy (rebrand).
        "camera-photo".into()
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
