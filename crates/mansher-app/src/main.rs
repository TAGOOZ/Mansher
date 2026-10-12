//! Mansher clothesline overlay — GTK4 + hybrid hover reveal.
//!
//! Sensors (see `mansher_core::showhide`):
//! - Niri/wlroots/KDE Wayland: 3px layer-shell strip at the top edge.
//!   Hover inside our own surface reveals — Wayland-legal, zero polling.
//! - Everywhere else: compositor keybind / shortcut spawns
//!   `mansher-app --toggle` (Unix-socket IPC) + 2.5s auto-peek on capture.
//!   X11 pointer polling and the GNOME extension D-Bus sensor are next.

mod cards;
mod clipboard;
mod clipwatch;
mod doctor;
mod ipc;
mod prefs;
mod settings;
mod strip_raw;
mod thumbs;
mod trash;
mod tray;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{mpsc, Arc, Mutex};

use std::time::Duration;

use anyhow::Result;
use gtk::prelude::*;
use gtk4 as gtk;
use mansher_core::{
    card_visual::{CardFrame, CardVisuals, UiHints},
    layout,
    showhide::{Command as ShCmd, Event as ShEv, Ms, Sensor, ShowHide},
    watch_inbox, Line,
};

const HOLD_MS: Ms = 450; // press-and-hold opens the annotator (upstream GrabArea)
const COPIED_MS: Ms = 1200; // "Copied" badge lifetime (upstream Line.copy)
const TICK_MS: u64 = 100;

fn now_ms() -> Ms {
    gtk::glib::monotonic_time() as Ms / 1000
}

fn detect_sensor() -> (Sensor, bool) {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let session = std::env::var("XDG_SESSION_TYPE")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let gnome = desktop.contains("gnome") || desktop.contains("ubuntu");
    if session == "wayland" && !gnome {
        #[cfg(feature = "layer-shell")]
        return (Sensor::LayerStrip, true);
        #[cfg(not(feature = "layer-shell"))]
        return (Sensor::None, false);
    }
    if session == "x11" {
        // Dead end noted by plan 010: X11Poll detected but no polling loop
        // behind it (untestable here).
        return (Sensor::X11Poll, false);
    }
    (Sensor::None, false)
}

// ---------------------------------------------------------------------------
// Shared UI state
// ---------------------------------------------------------------------------

struct App {
    line: Line,
    showhide: ShowHide,
    window: gtk::ApplicationWindow,
    cards: gtk::Box,
    scroll: gtk::ScrolledWindow,
    /// Follow the tail: new captures auto-scroll into view. Cleared the
    /// moment the user slides the row themselves, so browsing is never
    /// yanked away (re-armed by the next capture).
    follow_tail: Rc<Cell<bool>>,
    hint: gtk::Label,
    pointer_in: bool,
    /// Open card menu, if any. Its popup lives outside the window surface,
    /// so reaching for it reads as leaving the line — a stored menu blocks
    /// retract the same way `pointer_in` does until the menu closes.
    menu_pop: Option<gtk::Popover>,
    /// When the current menu opened. The tick grants a short grace so popup
    /// map negotiation never reads as the pointer being away.
    menu_opened_at: Ms,
    /// Stable popover parent: the root overlay, never rebuilt. Card columns
    /// churn on gust/badge/thumb edges; a menu parented to one dies
    /// silently (unparented, no `closed`) on the next refresh.
    popover_root: gtk::Widget,
    /// Pointer currently inside the hover strip. Re-fired to the state
    /// machine on every tick (see below).
    pointer_hot: bool,
    press: Option<(u128, u32, Ms)>,
    press_fired: bool,
    swallow_copy: Option<(u128, Ms)>,
    copied: Option<(u128, Ms)>,
    visuals: CardVisuals,
    hovered: Option<u128>,
    /// Last rendered frames: the tick re-renders only on phase edges, so
    /// keyframes animations are never restarted by a rebuild mid-flight.
    last_frames: Vec<CardFrame>,    /// Last painted card state, same order as the box children. Refresh
    /// diffs against this (not `last_frames`: the tick overwrites that gate
    /// copy before refresh runs, and it carries no badge/thumb info).
    rendered: Vec<cards::RenderedCard>,
    /// (copy, card identity) → live widget. The row renders three copies of
    /// the peg list for the endless wrap; never key on widget index.
    card_widgets: HashMap<(u8, u128), gtk::Widget>,
    /// (copy, card identity) → phase-class node for in-place restyles (press
    /// swaps classes on every live copy instead of rebuilding mid-gesture).
    card_outers: HashMap<(u8, u128), gtk::Overlay>,
    /// `photo_display_size` memo, pruned to live paths on every refresh.
    size_cache: RefCell<cards::SizeCache>,
    clip_guard: clipwatch::ClipGuard,
    /// Shared with the tray (single source for the Sounds checkmark).
    sound_on: Arc<Mutex<bool>>,
    /// Singleton Preferences window (tray item presents it).
    prefs: prefs::PrefsWindow,
    tick_count: u64,
    /// Reveal/tuck generation: Hide plays the 220ms tuck slide before
    /// unmapping, and a re-reveal inside that window cancels the hide.
    tuck_gen: u64,
    /// Arrival played per card id: the drop-in is a one-shot. Rebuilds
    /// (thumb landing, badge flip) must not replay it mid-flight, or the
    /// card visibly drops twice.
    arrived: HashSet<u128>,
    /// Row copies currently rendered (1 when the row fits the viewport,
    /// 3 when it overflows for the endless wrap). The tick recounts and
    /// refreshes on change.
    rendered_copies: u8,
}

type Shared = Rc<RefCell<App>>;

#[derive(Clone)]
enum Msg {
    NewCapture(PathBuf),
    ThumbsReady,
    /// Raw-Wayland strip: pointer entered the sensor band.
    StripEnter,
    /// Raw-Wayland strip: pointer left the sensor band.
    StripLeave,
    /// Tray / hotkey: toggle the line (stays open while empty).
    UiToggle,
    /// Tray: take every photo down (files stay on disk).
    UiClearAll,
    /// Tray: open the screenshots folder.
    UiOpenFolder,
    /// Tray: flip the Sounds checkmark.
    UiSoundsToggle,
    /// Tray: flip the Launch-at-login checkmark.
    UiAutostartToggle,
    /// Tray: open the Preferences window.
    UiPreferences,
    /// Prefs: live Max-items spin value.
    UiMaxItems(usize),
    /// Tray: quit.
    UiQuit,
}

fn apply_command(shared: &Shared, cmd: ShCmd) {
    // Never hold a borrow across set_visible: showing/hiding synthesizes
    // pointer enter/leave, whose handlers borrow too — holding one here
    // aborts the process with "RefCell already borrowed" the moment the
    // pointer happens to be over the window on a reveal/hide.
    match cmd {
        ShCmd::Noop => {}
        ShCmd::Reveal => {
            eprintln!("mansher: reveal");
            let (window, root) = {
                let mut app = shared.borrow_mut();
                app.tuck_gen += 1;
                (app.window.clone(), app.popover_root.clone())
            };
            window.set_visible(true);
            // Slide down on the upstream spring (0.42s); re-adding the class
            // restarts the keyframes run each reveal.
            root.remove_css_class("mtuck");
            root.remove_css_class("mreveal");
            root.add_css_class("mreveal");
        }
        ShCmd::Hide => {
            eprintln!("mansher: hide");
            shared.borrow_mut().menu_pop.take();
            let (window, root, gen) = {
                let mut app = shared.borrow_mut();
                app.tuck_gen += 1;
                let gen = app.tuck_gen;
                (
                    app.window.clone(),
                    app.popover_root.clone(),
                    gen,
                )
            };
            // Quick tuck slide (220ms ease-in) first; unmap after, unless a
            // re-reveal in between cancelled this generation.
            root.remove_css_class("mreveal");
            root.add_css_class("mtuck");
            let shared_hide = Rc::clone(shared);
            gtk::glib::timeout_add_local_once(Duration::from_millis(220), move || {
                let still = {
                    let app = shared_hide.borrow();
                    app.tuck_gen == gen && !app.showhide.revealed
                };
                if still {
                    root.remove_css_class("mtuck");
                    window.set_visible(false);
                }
            });
        }
    }
}

/// Ground truth for "pointer is still with the open menu": hit-test the
/// menu's own popup surface and the line window surface. Window enter/leave
/// are unreliable around popup map (the popup steals the pointer) and button
/// release (grab end recomputes hover), so neither may dismiss the menu.
/// The menu is modal-ish: it stays while the pointer is anywhere over our UI
/// (menu or card) and closes only once the pointer is off both — which also
/// lets the normal retract path hide the line. Unknown device or fresh popup
/// keeps it.
fn menu_pointer_on_menu(shared: &Shared) -> bool {
    let (pop, window, opened_ago) = {
        let app = shared.borrow();
        match app.menu_pop.clone() {
            Some(pop) => (
                pop,
                app.window.clone(),
                now_ms().saturating_sub(app.menu_opened_at),
            ),
            None => return false,
        }
    };
    if opened_ago < 300 {
        return true;
    }
    let device = gtk::gdk::Display::default()
        .and_then(|d| d.default_seat())
        .and_then(|s| s.pointer());
    let Some(device) = device else {
        return true;
    };
    let on_surface = |w: &gtk::Widget| {
        w.native()
            .and_then(|n| n.surface())
            .and_then(|s| s.device_position(&device))
            .is_some()
    };
    let menu_widget: gtk::Widget = pop.upcast();
    let window_widget: gtk::Widget = window.upcast();
    on_surface(&menu_widget) || on_surface(&window_widget)
}

fn send(shared: &Shared, ev: ShEv) {
    // TEMP flap-scope: every state-machine event with a timestamp.
    eprintln!("mansher: ev {ev:?} @{}", now_ms());
    let cmd = shared.borrow_mut().showhide.handle(ev);
    apply_command(shared, cmd);
}

fn hang_path(shared: &Shared, path: PathBuf) {
    let now = now_ms();
    let hung_id = shared.borrow_mut().line.hang(path.clone());
    if let Some(uuid) = hung_id {
        let tilt = shared
            .borrow()
            .line
            .pegs()
            .iter()
            .find(|p| p.id == uuid)
            .map(|p| p.tilt)
            .unwrap_or(0.0);
        shared.borrow_mut().visuals.hung(uuid.as_u128(), tilt, now);
        eprintln!("mansher: hung {}", path.display());
        shared.borrow().follow_tail.set(true);
        // Upstream Tink on hang (restores never come through here, so this
        // is always a genuinely new capture — no startup cacophony).
        settings::play_if_on("camera-shutter", &shared.borrow().sound_on.clone());
        send(shared, ShEv::Captured(now));
        cards::refresh(shared);
        // Immediate scroll for the already-laid-out case; the adjustment
        // `changed` handler below covers the relayout that follows.
        scroll_to_end(shared);
    }
}

/// Pin the row to its newest end. Called directly (geometry valid) and from
/// the adjustment `changed` handler (geometry pending) — both clamp, so
/// calling twice is harmless.
fn scroll_to_end(shared: &Shared) {
    let app = shared.borrow();
    let adj = app.scroll.hadjustment();
    adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
}

// ---------------------------------------------------------------------------
// Copy + discard glue (touch App/Line/Msg jointly; stay until a later plan
// earns their move)
// ---------------------------------------------------------------------------

fn do_copy(shared: &Shared, id: u128, path: &Path) {
    let hash = match clipboard::copy_to_clipboard(path) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("mansher: copy failed: {e:#}");
            return;
        }
    };
    {
        let mut app = shared.borrow_mut();
        *app.clip_guard.lock().unwrap() = Some(hash);
        let now = now_ms();
        app.copied = Some((id, now));
        app.visuals.copied(id, now);
    }
    eprintln!("mansher: copied {id} hash={hash:016x}");
    cards::refresh(shared);
}

/// Trash-then-drop for Discard: trash the file, drop the peg only on success.
/// Returns true when the peg left the line. Pure over `Line` (the caller
/// supplies `inbox`, already resolved via `is_in_inbox_by_u128`) so tests
/// observe peg retention without GTK or real dirs.
fn discard_peg(
    line: &mut Line,
    id: u128,
    inbox: bool,
    trash: impl Fn(&Path) -> Result<()>,
) -> bool {
    if inbox {
        if let Some(p) = line
            .pegs()
            .iter()
            .find(|p| p.id.as_u128() == id)
            .map(|p| p.path.clone())
        {
            if let Err(e) = trash(&p) {
                eprintln!("mansher: trash failed: {e:#}");
                return false;
            }
        }
    }
    line.drop_by_u128(id);
    true
}

fn do_discard(shared: &Shared, id: u128) {
    let dropped = {
        let mut app = shared.borrow_mut();
        let inbox = app.line.is_in_inbox_by_u128(id);
        discard_peg(&mut app.line, id, inbox, trash::trash_file)
    };
    if !dropped {
        return;
    }
    // Upstream Pop on drop; files stay or trash per above — never deleted outright.
    let sound_on = shared.borrow().sound_on.clone();
    let now = now_ms();
    {
        let mut app = shared.borrow_mut();
        app.press = None;
        app.press_fired = false;
        app.swallow_copy = Some((id, now));
        app.line.drop_by_u128(id);
        app.visuals.dropped(id, now);
    }
    settings::play_if_on("trash-empty", &sound_on);
    cards::refresh(shared);
}

#[cfg(feature = "layer-shell")]
fn init_layer(window: &gtk::ApplicationWindow, exclusive_zone: i32, width: i32, height: i32) {
    use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
    window.init_layer_shell();
    window.set_layer(Layer::Top);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, true);
    window.set_exclusive_zone(exclusive_zone);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_default_size(width, height);
}

/// Output width in px. Niri honors our layer-shell size instead of
/// stretching us, so the hardcoded 900 left the rope ending mid-screen
/// and the 600px strip sitting top-left. Measure like upstream sizes
/// its panel to the visible frame.
fn output_width() -> Option<i32> {
    let display = gtk::gdk::Display::default()?;
    let monitors = display.monitors();
    let obj = monitors.item(0)?;
    let monitor = obj.downcast::<gtk::gdk::Monitor>().ok()?;
    Some(monitor.geometry().width())
}

fn build_ui(
    app: &gtk::Application,
    rx: mpsc::Receiver<Msg>,
    ipc_rx: mpsc::Receiver<ipc::IpcEvent>,
    tx: mpsc::Sender<Msg>,
    clip_guard: clipwatch::ClipGuard,
    sound_on: Arc<Mutex<bool>>,
) {
    cards::apply_css();
    let (sensor, use_layer_shell) = detect_sensor();
    let out_w = output_width().unwrap_or(900);

    // Main clothesline window: full-width strip, hidden until revealed.
    let window = gtk::ApplicationWindow::new(app);
    window.add_css_class("mansher");
    window.set_title(Some("Mansher"));
    window.set_decorated(false);
    window.set_resizable(false);
    window.set_default_size(out_w, layout::PANEL_HEIGHT as i32);
    if use_layer_shell {
        #[cfg(feature = "layer-shell")]
        init_layer(&window, 0, out_w, layout::PANEL_HEIGHT as i32);
    } else {
        window.maximize();
    }

    let root = gtk::Overlay::new();
    root.set_halign(gtk::Align::Fill);
    root.set_valign(gtk::Align::Start);
    window.set_child(Some(&root));

    let bg = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bg.set_halign(gtk::Align::Fill);
    bg.set_hexpand(true);
    bg.set_size_request(out_w, layout::PANEL_HEIGHT as i32);
    bg.append(&cards::rope_widget(out_w));
    root.set_child(Some(&bg));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_halign(gtk::Align::Fill);
    content.set_valign(gtk::Align::Start);
    content.set_hexpand(true);
    content.set_margin_top(layout::CONTENT_TOP_OFFSET as i32);
    root.add_overlay(&content);

    // Sliding row: cards keep their fixed size and pan left/right on
    // overflow instead of shrinking. Wheel and two-finger scroll drive it
    // programmatically; EXTERNAL hides the scrollbar but keeps the
    // viewport+adjustment mechanics (NEVER disables scrolling entirely).
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_halign(gtk::Align::Fill);
    scroll.set_hexpand(true);
    scroll.set_policy(gtk::PolicyType::External, gtk::PolicyType::Never);
    scroll.set_overlay_scrolling(true);
    scroll.set_propagate_natural_height(true);
    content.append(&scroll);

    let cards = gtk::Box::new(gtk::Orientation::Horizontal, 24);
    cards.set_halign(gtk::Align::Center);
    cards.set_margin_start(24);
    cards.set_margin_end(24);
    cards.set_margin_bottom(6);
    scroll.set_child(Some(&cards));
    // Hover zoom/tilt paint outside card allocations; the auto-added viewport
    // clips to its bounds and slices the zoomed bottom off. Vertical
    // scrolling is impossible here (vpolicy Never), so letting overpaint
    // spill to the window costs nothing.
    if let Some(viewport) = scroll.child() {
        viewport.set_overflow(gtk::Overflow::Visible);
    }

    let follow_tail: Rc<Cell<bool>> = Rc::new(Cell::new(true));
    {
        // User takes the wheel: stop following the tail.
        let follow = Rc::clone(&follow_tail);
        let hadj = scroll.hadjustment();
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        wheel.connect_scroll(move |_, dx, dy| {
            follow.set(false);
            let d = if dx != 0.0 { dx } else { dy };
            // Discrete wheel notches arrive as ±1: jump about a card.
            // Smooth touchpad deltas are already in px: nudge lightly.
            let step = if d.fract() == 0.0 {
                layout::SPACING
            } else {
                4.0
            };
            let end = (hadj.upper() - hadj.page_size()).max(hadj.lower());
            hadj.set_value((hadj.value() + d * step).clamp(hadj.lower(), end));
            gtk::glib::Propagation::Stop
        });
        scroll.add_controller(wheel);
    }
    {
        // Relayout landed (new card, first reveal, restore): if we are
        // following the tail, pin to the newest end. Timing-proof: this
        // fires after GTK recomputes upper/page_size.
        let follow = Rc::clone(&follow_tail);
        scroll.hadjustment().connect_changed(move |adj| {
            if follow.get() {
                adj.set_value((adj.upper() - adj.page_size()).max(adj.lower()));
            }
        });
    }

    let hint = gtk::Label::new(Some("Take a screenshot and it will hang here"));
    hint.add_css_class("mansher-hint");
    hint.set_halign(gtk::Align::Center);
    content.append(&hint);

    let mut line = Line::new();
    // Flat capacity: the row slides, so fitting the screen no longer
    // decides how many survive — only a truly full line drops the oldest.
    line.max_items = settings::load_max_items();
    let shared: Shared = Rc::new(RefCell::new(App {
        line,
        showhide: ShowHide::new(sensor),
        window: window.clone(),
        cards: cards.clone(),
        scroll: scroll.clone(),
        follow_tail: Rc::clone(&follow_tail),
        hint: hint.clone(),
        pointer_in: false,
        menu_pop: None,
        menu_opened_at: 0,
        popover_root: root.clone().upcast(),
        pointer_hot: false,
        press: None,
        press_fired: false,
        swallow_copy: None,
        copied: None,
        visuals: CardVisuals::new(),
        hovered: None,
        last_frames: Vec::new(),
        rendered: Vec::new(),
        card_widgets: HashMap::new(),
        card_outers: HashMap::new(),
        size_cache: RefCell::new(HashMap::new()),
        clip_guard: clip_guard.clone(),
        sound_on: Arc::clone(&sound_on),
        prefs: prefs::PrefsWindow::new(),
        tick_count: 0,
        tuck_gen: 0,
        arrived: HashSet::new(),
        rendered_copies: 3,
    }));

    // Upstream itemsChanged: a non-empty line is wanted from the start.
    // Without this, hover can never reveal a restored session — HotEnter
    // is gated on presentable() (wanted && !blocked), and wanted was only
    // ever set by a fresh capture or toggle.
    if shared.borrow().line.live_count() > 0 {
        shared.borrow_mut().showhide.wanted = true;
    }

    // Thumbnail worker: newest-first batches, one refresh per batch.
    // Restored pegs queue here too, so first paint already has thumbs.
    let (thumb_tx, thumb_rx) = mpsc::channel::<PathBuf>();
    {
        let tx = tx.clone();
        std::thread::spawn(move || loop {
            let Ok(first) = thumb_rx.recv() else { return };
            let mut batch = vec![first];
            batch.extend(thumb_rx.try_iter());
            batch.reverse();
            let mut made = false;
            for p in batch {
                if thumbs::ensure_thumb(&p).is_some() {
                    made = true;
                }
            }
            if made {
                let _ = tx.send(Msg::ThumbsReady);
            }
        });
    }
    for p in shared
        .borrow()
        .line
        .pegs()
        .iter()
        .map(|p| p.path.clone())
        .collect::<Vec<_>>()
    {
        let _ = thumb_tx.send(p);
    }

    // Pointer inside the line zone (unpins + blocks retract while inside).
    let motion = gtk::EventControllerMotion::new();
    {
        let shared = Rc::clone(&shared);
        motion.connect_enter(move |_, _, _| {
            shared.borrow_mut().pointer_in = true;
            send(&shared, ShEv::InsideLine);
        });
    }
    {
        let shared = Rc::clone(&shared);
        motion.connect_leave(move |_| {
            eprintln!("[DEBUG-m2] root leave");
            shared.borrow_mut().pointer_in = false;
        });
    }
    root.add_controller(motion);

    // Hover sensor: raw-Wayland strip (see strip_raw.rs), no GTK window.
    // A GTK layer-shell strip never drew here — gtk-layer-shell snapshots
    // the size at map time and stuck ours at a 200px default forever —
    // so the sensor speaks protocol directly on its own thread and reports
    // over Msg. Same dwell + zone logic downstream.
    if use_layer_shell {
        strip_raw::start(tx.clone(), out_w);
    }

    cards::refresh(&shared);
    window.set_visible(false);

    // 100ms tick: drain IPC + watcher, hold-to-annotate, badge expiry,
    // existence prune, away-from-line retract timing.
    gtk::glib::timeout_add_local(Duration::from_millis(TICK_MS), move || {
        let now = now_ms();
        let mut dirty = false;

        for msg in rx.try_iter() {
            match msg {
                Msg::NewCapture(path) => {
                    let _ = thumb_tx.send(path.clone());
                    hang_path(&shared, path);
                }
                Msg::ThumbsReady => {
                    dirty = true;
                }
                // Raw strip events drive the same dwell + zone logic the
                // GTK strip used (enter arms dwell, zone blocks retract).
                Msg::StripEnter => {
                    let mut app = shared.borrow_mut();
                    app.pointer_hot = true;
                    app.pointer_in = true;
                    drop(app);
                    send(&shared, ShEv::HotEnter(now));
                    send(&shared, ShEv::InsideLine);
                }
                Msg::StripLeave => {
                    let mut app = shared.borrow_mut();
                    app.pointer_hot = false;
                    // Zone union (upstream: the zone runs up to the top of
                    // the screen, menu bar included): leaving the strip does
                    // NOT mean leaving the zone — when revealed, the pointer
                    // is necessarily onto the line (the strip overlaps it).
                    // Clearing here made every reveal hide itself ~600ms
                    // later: reveal → main maps over strip → strip-leave
                    // clears inside → retract → hide → re-enter → flashing.
                    if !app.showhide.revealed {
                        app.pointer_in = false;
                    }
                    drop(app);
                    send(&shared, ShEv::HotLeave);
                }
                // Tray actions (same thread as everything else).
                Msg::UiToggle => {
                    send(&shared, ShEv::Toggle);
                }
                Msg::UiClearAll => {
                    // Upstream clear(): everything leaves at once; the first plays
                    // Pop, the rest go quietly. Files stay on disk.
                    let ids: Vec<u128> = {
                        let app = shared.borrow();
                        app.line.pegs().iter().map(|p| p.id.as_u128()).collect()
                    };
                    let sound_on = shared.borrow().sound_on.clone();
                    let mut first = true;
                    for id in &ids {
                        shared.borrow_mut().line.drop_by_u128(*id);
                        shared.borrow_mut().visuals.dropped(*id, now_ms());
                        if first {
                            first = false;
                            settings::play_if_on("trash-empty", &sound_on);
                        }
                    }
                    dirty = true;
                }
                Msg::UiOpenFolder => {
                    if let Ok(inbox) = mansher_core::inbox_dir() {
                        trash::open_path(&inbox);
                    }
                }
                Msg::UiSoundsToggle => {
                    let on = {
                        let app = shared.borrow();
                        !app.sound_on.lock().map(|g| *g).unwrap_or(true)
                    };
                    *shared.borrow().sound_on.lock().unwrap() = on;
                    settings::save_sound_on(on);
                }
                Msg::UiAutostartToggle => {
                    settings::set_autostart(!settings::autostart_desktop().exists());
                }
                Msg::UiPreferences => {
                    let (sounds, autostart, max_items) = {
                        let app = shared.borrow();
                        (
                            app.sound_on.lock().map(|g| *g).unwrap_or(true),
                            settings::autostart_desktop().exists(),
                            app.line.max_items,
                        )
                    };
                    let doctor = crate::doctor::Report::collect().render();
                    let parent = shared.borrow().window.clone();
                    shared
                        .borrow_mut()
                        .prefs
                        .show(&parent, sounds, autostart, max_items, &doctor, &tx);
                }
                Msg::UiMaxItems(n) => {
                    shared.borrow_mut().line.max_items = settings::save_max_items(n);
                }
                Msg::UiQuit => {
                    std::process::exit(0);
                }
            }
        }
        for ev in ipc_rx.try_iter() {
            match ev {
                ipc::IpcEvent::Toggle => {
                    eprintln!("mansher: ipc toggle received");
                    send(&shared, ShEv::Toggle);
                }
                ipc::IpcEvent::Preferences => {
                    let _ = tx.send(Msg::UiPreferences);
                }
            }
        }

        // Strip dwell re-fire. Upstream polls the global pointer at 30Hz
        // and reveals after a 250ms rest; Wayland forbids global queries,
        // so while the pointer sits inside the strip we re-send HotEnter
        // at 10Hz — the state machine's dwell then completes and reveals.
        // (Read the flag first: the borrow guard must drop before send()
        // takes a mutable borrow, or the tick panics on every hover.)
        let hot = shared.borrow().pointer_hot;
        if hot {
            send(&shared, ShEv::HotEnter(now));
        }

        // Hold 450ms -> annotate (upstream GrabArea.holdDuration).
        let hold_fire: Option<PathBuf> = {
            let app = shared.borrow();
            match app.press {
                Some((id, _, t)) if !app.press_fired && now.saturating_sub(t) >= HOLD_MS => app
                    .line
                    .pegs()
                    .iter()
                    .find(|p| p.id.as_u128() == id)
                    .map(|p| p.path.clone()),
                _ => None,
            }
        };
        if let Some(path) = hold_fire {
            let mut app = shared.borrow_mut();
            app.press_fired = true;
            app.press = None;
            drop(app);
            trash::annotate_path(&path);
        }

        // Copied badge expiry.
        {
            let mut app = shared.borrow_mut();
            if let Some((_, t)) = app.copied {
                if now.saturating_sub(t) >= COPIED_MS {
                    app.copied = None;
                    dirty = true;
                }
            }
        }

        // Existence prune about once a second (covers moves/deletes).
        {
            let before = shared.borrow().line.live_count();
            shared.borrow_mut().tick_count += 1;
            let tick = shared.borrow().tick_count;
            if tick.is_multiple_of(10) {
                shared.borrow_mut().line.prune();
                if shared.borrow().line.live_count() != before {
                    dirty = true;
                }
            }
        }

        // Away-from-line retract timing while revealed.
        // Menu dismissal from pointer ground truth (see
        // `menu_pointer_on_menu`): only the pointer truly off the menu
        // closes it. Runs before the retract block so the same tick can
        // retract once the menu is gone.
        if shared.borrow().menu_pop.is_some() && !menu_pointer_on_menu(&shared) {
            eprintln!("[DEBUG-m2] menu away-dismiss");
            let old = shared.borrow_mut().menu_pop.take();
            if let Some(old) = old {
                old.popdown();
            }
        }
        {
            let (revealed, inside, busy, menu_open) = {
                let app = shared.borrow();
                (
                    app.showhide.revealed,
                    app.pointer_in,
                    app.showhide.is_busy(),
                    app.menu_pop.is_some(),
                )
            };
            if revealed {
                if inside || busy || menu_open {
                    if menu_open && !inside && !busy {
                        eprintln!("[DEBUG-m2] menu-hold: revealed+menu, pointer away");
                    }
                    send(&shared, ShEv::InsideLine);
                } else {
                    send(&shared, ShEv::OutsideLine(now));
                }
            }
        }

        // Card motion edges: arrival settling, gust windows, nudge expiry,
        // hover and press all surface as frame changes; steady phases cost
        // one comparison and no rebuild.
        {
            let pegs = shared.borrow().line.pegs().to_vec();
            let mut app = shared.borrow_mut();
            let hints = UiHints {
                now,
                hovered: app.hovered,
                pressed: app.press.map(|(id, _, _)| id),
                revealed: app.showhide.revealed,
            };
            let frames = app.visuals.frames(&pegs, hints);
            if frames != app.last_frames {
                app.last_frames = frames;
                dirty = true;
            }
            // Row copies: a short row fits the viewport (single copy, no
            // visible duplicates); a long one needs all three for the wrap.
            // Recount here so resize/overflow flips rebuild once, not never.
            let copies = crate::cards::copy_count(
                &app.rendered,
                &app.size_cache,
                app.scroll.allocated_width(),
            );
            if copies != app.rendered_copies {
                dirty = true;
            }
        }

        if dirty {
            cards::refresh(&shared);
        }

        // Endless row wrap: three copies render in `cards` and the user lives
        // on the middle one; a drift into an outer copy shifts the adjustment
        // by exactly one copy pitch (see cards::wrap_value). follow_tail still
        // owns the newest end; this only recenters wheel drifters.
        {
            // Measure the real copy geometry from live widgets: card widths
            // vary with aspect ratio, so n*SPACING overshoots and the
            // downward wrap threshold lands past the true end (scroll-down
            // wall). Copy 1's start is the wrap band's lower edge.
            // No wrap while following the tail (the pin owns the newest end;
            // wrapping would yank it away and fight the changed handler at
            // 10Hz) or while a menu is open (the popover would detach from
            // its card).
            let hold = shared.borrow().follow_tail.get() || shared.borrow().menu_pop.is_some();
            let measured: Option<(f64, f64)> = (|| {
                if hold {
                    return None;
                }
                let app = shared.borrow();
                let id = app.card_outers.keys().find(|k| k.0 == 0).map(|k| k.1)?;
                let w0 = app.card_outers.get(&(0, id))?.clone();
                let w1 = app.card_outers.get(&(1, id))?.clone();
                let cards_box = app.cards.clone();
                drop(app);
                let (x0, _) = w0.translate_coordinates(&cards_box, 0.0, 0.0)?;
                let (x1, _) = w1.translate_coordinates(&cards_box, 0.0, 0.0)?;
                (x1 > x0).then_some((x1, x1 - x0))
            })();
            if let Some((mid_lo, pitch)) = measured {
                let adj = shared.borrow().scroll.hadjustment();
                let w = cards::wrap_value(
                    adj.value(),
                    adj.lower(),
                    adj.upper(),
                    adj.page_size(),
                    mid_lo,
                    pitch,
                );
                if w != adj.value() {
                    adj.set_value(w);
                }
            }
        }
        gtk::glib::ControlFlow::Continue
    });

    window.present();
    window.set_visible(false);
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Read-only report: no GTK, no primary acquire, so this works while the
    // daemon runs and with no display.
    if args.iter().any(|a| a == "--doctor") {
        print!("{}", crate::doctor::Report::collect().render());
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--copy-test") {
        // Headless proof of the copy path (same payload the click handler
        // serves): offer the image in-process for 10s, verify bytes with
        // wl-paste outside while this runs.
        let file = args.get(i + 1).cloned().unwrap_or_default();
        if !Path::new(&file).is_file() {
            anyhow::bail!("--copy-test needs an existing image file, got {file:?}");
        }
        if let Ok(img) = crate::thumbs::bounded_open(Path::new(&file)) {
            eprintln!("decode-dims {}x{}", img.width(), img.height());
        }
        let (payload, hash) = clipboard::payload_for(Path::new(&file))?;
        println!("copied-ok hash={hash:016x}");
        // Same path the click handler runs: serve in-process on a thread,
        // main thread just keeps the process alive like the running app.
        clipboard::serve_forever(payload);
        std::thread::sleep(std::time::Duration::from_secs(10));
        return Ok(());
    }
    if args.iter().any(|a| a == "--toggle") && ipc::send_toggle() {
        return Ok(());
    }
    // Tray-less sessions open Preferences the same way keybinds toggle.
    if args.iter().any(|a| a == "--preferences") && ipc::send_preferences() {
        return Ok(());
    }
    // Single fd handoff: the hold keeps the bound socket alive from acquire
    // to listener, so two starters can never both believe they are primary.
    let hold = match ipc::acquire_primary() {
        ipc::PrimaryResult::Primary(hold) => Some(hold),
        ipc::PrimaryResult::Secondary => {
            // Another instance is alive: deliver this starter's request.
            if args.iter().any(|a| a == "--preferences") {
                ipc::send_preferences();
            } else {
                ipc::send_toggle();
            }
            return Ok(());
        }
        ipc::PrimaryResult::Unavailable => None,
    };

    let (tx, rx) = mpsc::channel::<Msg>();
    let (ipc_tx, ipc_rx) = mpsc::channel::<ipc::IpcEvent>();
    let clip_guard: clipwatch::ClipGuard = Arc::new(Mutex::new(None));
    let sound_on = Arc::new(Mutex::new(settings::load_sound_on()));

    // StatusNotifier tray (menu parity + Quit). Needs a watcher (present
    // here); without one it logs and everything else keeps working.
    tray::start(tx.clone(), Arc::clone(&sound_on));

    // --toggle listener on the held socket (absent when standalone).
    if let Some(hold) = hold {
        ipc::start_listener(hold, ipc_tx);
    }

    // Clipboard screenshot capture (grim-piped-to-wl-copy writes no file).
    clipwatch::start(tx.clone(), Arc::clone(&clip_guard));

    // Inbox watcher thread (inotify; Watchers live here).
    let watch_tx = tx.clone();
    std::thread::spawn(move || match watch_inbox() {
        Ok((_watchers, inbox_rx)) => {
            for path in inbox_rx {
                let _ = watch_tx.send(Msg::NewCapture(path));
            }
        }
        Err(e) => eprintln!("mansher: inbox watch failed: {e:#}"),
    });

    let app = gtk::Application::new(Some("io.github.mansher.Mansher"), Default::default());
    // activate is Fn (may fire more than once): hand the channels over on first call.
    let rx_cell = Rc::new(RefCell::new(Some(rx)));
    let ipc_cell = Rc::new(RefCell::new(Some(ipc_rx)));
    let sound_cell = Rc::new(RefCell::new(Some(Arc::clone(&sound_on))));
    app.connect_activate(move |app| {
        let rx = rx_cell.borrow_mut().take();
        let ipc_rx = ipc_cell.borrow_mut().take();
        let sound_on = sound_cell.borrow_mut().take();
        let (Some(rx), Some(ipc_rx), Some(sound_on)) = (rx, ipc_rx, sound_on) else {
            eprintln!("mansher: duplicate activate ignored");
            return;
        };
        build_ui(
            app,
            rx,
            ipc_rx,
            tx.clone(),
            Arc::clone(&clip_guard),
            sound_on,
        );
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mansher-app-test-{}-{n}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    static STATE_LOCK: Mutex<()> = Mutex::new(());

    /// `Line::hang/drop` persist via `save()`; point state at a temp dir so
    /// these tests never touch the user's real line.json (mirrors core's
    /// `with_state_dir`).
    fn with_state_dir(f: impl FnOnce()) {
        let _guard = STATE_LOCK.lock().unwrap();
        let dir = temp_dir("state");
        std::env::set_var("MANSHER_STATE_DIR", &dir);
        f();
        std::env::remove_var("MANSHER_STATE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discard_failure_keeps_peg() {
        with_state_dir(|| {
            let work = temp_dir("discard-fail");
            let src = work.join("shot.png");
            std::fs::write(&src, b"pixels").unwrap();
            let mut line = Line::default();
            line.max_items = 24;
            let id = line.hang(src.clone()).unwrap().as_u128();
            let dropped = discard_peg(&mut line, id, true, |_| Err(anyhow::anyhow!("EACCES")));
            assert!(!dropped);
            assert!(line.pegs().iter().any(|p| p.id.as_u128() == id));
            assert!(src.exists());
            let _ = std::fs::remove_dir_all(&work);
        });
    }

    #[test]
    fn discard_success_trashes_and_drops_peg() {
        with_state_dir(|| {
            let work = temp_dir("discard-ok");
            let src = work.join("shot.png");
            std::fs::write(&src, b"pixels").unwrap();
            let data_home = work.join("data");
            let mut line = Line::default();
            line.max_items = 24;
            let id = line.hang(src.clone()).unwrap().as_u128();
            let dropped = discard_peg(&mut line, id, true, |p| trash::trash_file_in(p, &data_home));
            assert!(dropped);
            assert!(line.pegs().is_empty());
            assert!(!src.exists());
            assert!(data_home.join("Trash/files/shot.png").exists());
            let _ = std::fs::remove_dir_all(&work);
        });
    }

    #[test]
    fn discard_external_drops_without_trash() {
        with_state_dir(|| {
            let work = temp_dir("discard-ext");
            let src = work.join("shot.png");
            std::fs::write(&src, b"pixels").unwrap();
            let mut line = Line::default();
            line.max_items = 24;
            let id = line.hang(src.clone()).unwrap().as_u128();
            let dropped = discard_peg(&mut line, id, false, |_| {
                panic!("external discard must not trash")
            });
            assert!(dropped);
            assert!(line.pegs().is_empty());
            assert!(src.exists());
            let _ = std::fs::remove_dir_all(&work);
        });
    }
}
