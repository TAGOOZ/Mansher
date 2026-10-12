//! Preferences window: General / Environment / About notebook.
//!
//! Singleton: `App` owns one `PrefsWindow`, `show` presents the existing
//! window when there is one. Callbacks only send [`Msg`], mirroring the
//! tray — the GTK tick applies everything, so this module never mutates
//! the line itself.

use std::sync::mpsc;

use gtk::prelude::*;
use gtk4 as gtk;

use crate::Msg;

pub struct PrefsWindow {
    window: Option<gtk::Window>,
    sounds: Option<gtk::Switch>,
    autostart: Option<gtk::Switch>,
    max_items: Option<gtk::SpinButton>,
    doctor_buf: Option<gtk::TextBuffer>,
}

impl PrefsWindow {
    pub fn new() -> Self {
        Self {
            window: None,
            sounds: None,
            autostart: None,
            max_items: None,
            doctor_buf: None,
        }
    }

    pub fn show(
        &mut self,
        parent: &gtk::ApplicationWindow,
        sounds_on: bool,
        autostart_on: bool,
        max_items: usize,
        doctor_text: &str,
        tx: &mpsc::Sender<Msg>,
    ) {
        if self.window.is_none() {
            self.build(parent, tx);
        }
        if let Some(s) = &self.sounds {
            s.set_active(sounds_on);
        }
        if let Some(s) = &self.autostart {
            s.set_active(autostart_on);
        }
        if let Some(s) = &self.max_items {
            s.set_value(max_items as f64);
        }
        if let Some(b) = &self.doctor_buf {
            b.set_text(doctor_text);
        }
        if let Some(w) = &self.window {
            w.present();
        }
    }

    fn build(&mut self, parent: &gtk::ApplicationWindow, tx: &mpsc::Sender<Msg>) {
        let window = gtk::Window::new();
        window.set_title(Some("Mansher Preferences"));
        window.set_transient_for(Some(parent));
        window.set_default_size(460, 380);
        window.set_hide_on_close(true);

        let notebook = gtk::Notebook::new();
        let (general, sounds, autostart, max_items) = Self::general_page(tx);
        notebook.append_page(&general, Some(&gtk::Label::new(Some("General"))));
        let doctor_buf = Self::environment_page(&notebook);
        notebook.append_page(&Self::about_page(), Some(&gtk::Label::new(Some("About"))));
        window.set_child(Some(&notebook));

        self.window = Some(window);
        self.sounds = Some(sounds);
        self.autostart = Some(autostart);
        self.max_items = Some(max_items);
        self.doctor_buf = Some(doctor_buf);
    }

    fn switch_row(label: &str, tx: &mpsc::Sender<Msg>, msg: Msg) -> (gtk::Box, gtk::Switch) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let name = gtk::Label::new(Some(label));
        name.set_halign(gtk::Align::Start);
        name.set_hexpand(true);
        let switch = gtk::Switch::new();
        switch.set_halign(gtk::Align::End);
        row.append(&name);
        row.append(&switch);
        // state-set fires on user clicks only, so the refresh in `show`
        // never sends a Msg back.
        let tx = tx.clone();
        switch.connect_state_set(move |_, _| {
            let _ = tx.send(msg.clone());
            gtk::glib::Propagation::Proceed
        });
        (row, switch)
    }

    fn general_page(
        tx: &mpsc::Sender<Msg>,
    ) -> (gtk::Box, gtk::Switch, gtk::Switch, gtk::SpinButton) {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        page.set_margin_top(16);
        page.set_margin_bottom(16);
        page.set_margin_start(16);
        page.set_margin_end(16);

        let (sounds_row, sounds) = Self::switch_row("Sounds", tx, Msg::UiSoundsToggle);
        let (autostart_row, autostart) =
            Self::switch_row("Launch at login", tx, Msg::UiAutostartToggle);
        page.append(&sounds_row);
        page.append(&autostart_row);

        let max_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let max_label = gtk::Label::new(Some("Max items"));
        max_label.set_halign(gtk::Align::Start);
        max_label.set_hexpand(true);
        let spin = gtk::SpinButton::with_range(4.0, 48.0, 1.0);
        spin.set_digits(0);
        spin.set_numeric(true);
        spin.set_halign(gtk::Align::End);
        max_row.append(&max_label);
        max_row.append(&spin);
        page.append(&max_row);
        let tx_max = tx.clone();
        spin.connect_value_changed(move |s| {
            let _ = tx_max.send(Msg::UiMaxItems(s.value() as usize));
        });

        let open_btn = gtk::Button::with_label("Open data folder");
        let tx_open = tx.clone();
        open_btn.connect_clicked(move |_| {
            let _ = tx_open.send(Msg::UiOpenFolder);
        });
        page.append(&open_btn);

        let clear_btn = gtk::Button::with_label("Clear all");
        let tx_clear = tx.clone();
        clear_btn.connect_clicked(move |_| {
            let _ = tx_clear.send(Msg::UiClearAll);
        });
        page.append(&clear_btn);

        (page, sounds, autostart, spin)
    }

    fn environment_page(notebook: &gtk::Notebook) -> gtk::TextBuffer {
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_vexpand(true);
        scroll.set_hexpand(true);
        let view = gtk::TextView::new();
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_monospace(true);
        view.set_wrap_mode(gtk::WrapMode::None);
        scroll.set_child(Some(&view));
        notebook.append_page(&scroll, Some(&gtk::Label::new(Some("Environment"))));
        view.buffer()
    }

    fn about_page() -> gtk::Box {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
        page.set_margin_top(16);
        page.set_margin_bottom(16);
        page.set_margin_start(16);
        page.set_margin_end(16);
        let name = gtk::Label::new(Some(&format!("Mansher {}", env!("CARGO_PKG_VERSION"))));
        name.set_halign(gtk::Align::Start);
        let license = gtk::Label::new(Some("License: MIT"));
        license.set_halign(gtk::Align::Start);
        let link = gtk::LinkButton::new("https://github.com/TAGOOZ/Mansher");
        link.set_halign(gtk::Align::Start);
        let privacy = gtk::Label::new(Some(
            "Private by design: no account, no network, no analytics.",
        ));
        privacy.set_halign(gtk::Align::Start);
        privacy.set_wrap(true);
        page.append(&name);
        page.append(&license);
        page.append(&link);
        page.append(&privacy);
        page
    }
}
