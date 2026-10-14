//! Card view + gestures (upstream GrabArea / PeggedView).
//!
//! Diff-and-patch row: steady-state ticks must not rebuild surviving cards.
//! Photo sizes come from the mtime-keyed cache, tilt from the clip bucket,
//! motion from the phase table. The right-click menu, rope strip, and glass
//! CSS live here too; `App`/`Msg` stay in the crate root and are borrowed
//! via `crate::`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use gtk::prelude::*;
use gtk4 as gtk;
use mansher_core::{
    card_visual::{CardFrame, Phase, UiHints},
    layout,
};

use crate::settings::{pictures_dir, play_if_on};
use crate::thumbs;
use crate::trash::{annotate_path, keep_in_dir, open_folder, open_path, trash_file};
use crate::{do_copy, do_discard, now_ms, Shared, HOLD_MS};

/// Counts real `image_dimensions` file decodes; refresh logs and resets it
/// per pass so the log shows what each refresh actually cost.
static SIZE_DECODES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn refresh(shared: &Shared) {
    // Diff-and-patch: steady-state ticks (badge expiry, thumb arrival) must
    // not destroy surviving cards. Anything visible-affecting lives in
    // RenderedCard; equal paint state touches no widget.
    let started = std::time::Instant::now();
    let now = now_ms();
    let pegs = shared.borrow().line.pegs().to_vec();
    let items: Vec<RenderedCard> = {
        let mut app = shared.borrow_mut();
        let hints = UiHints {
            now,
            hovered: app.hovered,
            pressed: app.press.map(|(id, _, _)| id),
            revealed: app.showhide.revealed,
        };
        app.visuals
            .frames(&pegs, hints)
            .into_iter()
            .map(|f| {
                let path = app
                    .line
                    .pegs()
                    .iter()
                    .find(|p| p.id.as_u128() == f.id)
                    .map(|p| p.path.clone())
                    .unwrap_or_default();
                let badge = app.copied.map(|(c, _)| c == f.id).unwrap_or(false);
                let has_thumb = thumbs::cached_thumb(&path).is_some();
                RenderedCard {
                    frame: f,
                    path,
                    badge,
                    has_thumb,
                }
            })
            .collect()
    };
    let show_hint = shared.borrow().line.pegs().is_empty();
    let (cards_box, hint_label) = {
        let app = shared.borrow();
        (app.cards.clone(), app.hint.clone())
    };
    let (built, removed, full) = {
        let mut app = shared.borrow_mut();
        app.last_frames = items.iter().map(|r| r.frame.clone()).collect();
        let old = std::mem::take(&mut app.rendered);
        let mut map = std::mem::take(&mut app.card_widgets);
        let mut outers = std::mem::take(&mut app.card_outers);
        // Release the App borrow before building widgets: card_widget
        // re-borrows shared for hover state and the size cache.
        drop(app);
        let stats = patch_cards(&cards_box, &mut map, &mut outers, &old, &items, shared);
        let mut app = shared.borrow_mut();
        app.card_widgets = map;
        app.card_outers = outers;
        app.rendered = items;
        // Drop cache entries for files that left the line (≤24 live pegs).
        let live: HashSet<&Path> = app.rendered.iter().map(|r| r.path.as_path()).collect();
        app.size_cache
            .borrow_mut()
            .retain(|p, _| live.contains(p.as_path()));
        stats
    };
    hint_label.set_visible(show_hint);
    eprintln!(
        "mansher: refresh {} cards ({} built, {} removed{}) in {:?}, {} size decodes",
        shared.borrow().rendered.len(),
        built,
        removed,
        if full { ", full" } else { "" },
        started.elapsed(),
        SIZE_DECODES.swap(0, Ordering::Relaxed),
    );
}

/// Press began since the last paint: everything matches except the phase
/// flipped to Pressed. The widget must NOT rebuild — its gesture controllers
/// are live, and a rebuild destroys them mid-sequence (press without release,
/// click without fire). The caller swaps phase classes in place instead.
fn press_onset(old: &RenderedCard, new: &RenderedCard) -> bool {
    if old.frame.id != new.frame.id
        || old.frame.phase == Phase::Pressed
        || new.frame.phase != Phase::Pressed
    {
        return false;
    }
    let mut masked = new.clone();
    masked.frame.phase = old.frame.phase;
    masked == *old
}

/// Diff old paint state against new; touch only what changed.
///
/// A full-order rebuild happens ONLY when membership order changed beyond
/// append (prune/reorder): survivors must keep screen position, and box
/// order is positional, so anything subtler risks misplaced cards.
/// Steady-state appends and per-card updates never rebuild survivors.
///
/// Changed-but-present cards (badge/phase/thumb flip) rebuild just that
/// card: no cheap in-place path exists (the built widget exposes no handles
/// for its badge/thumb children), and a single-card rebuild cannot stale
/// its neighbors — every `move` closure in `card_widget` captures only
/// card-stable values (`id`, `path`, `shared`) plus an upgrade-guarded weak
/// to its own cross, which dies with the card.
fn patch_cards(
    cards: &gtk::Box,
    map: &mut HashMap<u128, gtk::Widget>,
    outers: &mut HashMap<u128, gtk::Overlay>,
    old: &[RenderedCard],
    new: &[RenderedCard],
    shared: &Shared,
) -> (usize, usize, bool) {
    let old_ids: Vec<u128> = old.iter().map(|r| r.frame.id).collect();
    // One-shot arrivals: forget ids that left the line so the set stays
    // bounded to live pegs.
    {
        let live: HashSet<u128> = new.iter().map(|r| r.frame.id).collect();
        shared.borrow_mut().arrived.retain(|id| live.contains(id));
    }
    let survivors_in_old_order: Vec<u128> = old_ids
        .iter()
        .filter(|id| new.iter().any(|r| r.frame.id == **id))
        .copied()
        .collect();
    let new_ids: Vec<u128> = new.iter().map(|r| r.frame.id).collect();
    // Survivors keep relative order (ids are unique, so a prefix match means
    // every survivor sits up front and everything after is novel).
    if new_ids.starts_with(&survivors_in_old_order) {
        let mut built = 0;
        let mut removed = 0;
        // Drop pegs that left the line.
        let stale: Vec<u128> = map
            .keys()
            .filter(|id| !new_ids.contains(id))
            .copied()
            .collect();
        for key in stale {
            if let Some(w) = map.remove(&key) {
                cards.remove(&w);
                removed += 1;
            }
            outers.remove(&key);
        }
        let mut prev: Option<gtk::Widget> = None;
        for r in new {
            let id = r.frame.id;
            let old_o = old.iter().find(|o| o.frame.id == id);
            let same = old_o.is_some_and(|o| *o == *r) && map.contains_key(&id);
            if same {
                prev = map.get(&id).cloned();
                continue;
            }
            if let Some(o) = old_o {
                if press_onset(o, r) {
                    // Press began: swap phase classes in place.
                    // Rebuilding here would destroy the gesture's own
                    // widget — press without release, click without fire.
                    if let Some(outer) = outers.get(&id) {
                        for cls in phase_classes(&o.frame) {
                            outer.remove_css_class(&cls);
                        }
                        for cls in phase_classes(&r.frame) {
                            outer.add_css_class(&cls);
                        }
                    }
                    prev = map.get(&id).cloned();
                    continue;
                }
            }
            if let Some(old_w) = map.remove(&id) {
                cards.remove(&old_w);
            }
            outers.remove(&id);
            let (w, outer) = card_widget(shared, &r.frame, &r.path, r.badge);
            match &prev {
                Some(p) => cards.insert_child_after(&w, Some(p)),
                None => cards.insert_child_after(&w, None::<&gtk::Widget>),
            }
            map.insert(id, w.clone());
            outers.insert(id, outer);
            prev = Some(w);
            built += 1;
        }
        debug_assert_eq!(map.len(), new.len());
        (built, removed, false)
    } else {
        let mut cleared = 0;
        while let Some(child) = cards.first_child() {
            cards.remove(&child);
            cleared += 1;
        }
        map.clear();
        outers.clear();
        let mut built = 0;
        for r in new {
            let (w, outer) = card_widget(shared, &r.frame, &r.path, r.badge);
            cards.append(&w);
            map.insert(r.frame.id, w);
            outers.insert(r.frame.id, outer);
            built += 1;
        }
        (built, cleared, true)
    }
}

/// Painted state of one card: everything visible-affecting that `CardFrame`
/// alone does not carry (badge pill, thumb arrival, peg path).
#[derive(Clone, PartialEq)]
pub(crate) struct RenderedCard {
    frame: CardFrame,
    path: PathBuf,
    badge: bool,
    has_thumb: bool,
}

/// `photo_display_size` memo: path → (mtime nanos, byte len, display size).
/// Invalidation inputs mirror the thumb key (`thumbs::key_for`). Misses are
/// never cached: a missing file may appear on the next pass.
pub(crate) type SizeCache = HashMap<PathBuf, (u128, u64, (i32, i32))>;

fn cache_key(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((mtime, meta.len()))
}

/// Upstream PeggedView.photoSize: the photo fits inside the layout photo box
/// keeping proportions, so the white border hugs it whether wide or tall.
/// Without this cap, landscape shots expand to natural width and the row
/// overflows the output.
pub(crate) fn photo_display_size(cache: &RefCell<SizeCache>, path: &Path) -> (i32, i32) {
    const MAX_W: f64 = layout::PHOTO_MAX_W as f64;
    const MAX_H: f64 = layout::PHOTO_MAX_H as f64;
    let fallback = (MAX_W as i32, MAX_H as i32);
    let fit = |w: u32, h: u32| {
        if w > 0 && h > 0 {
            let s = (MAX_W / w as f64).min(MAX_H / h as f64);
            (1.max((w as f64 * s) as i32), 1.max((h as f64 * s) as i32))
        } else {
            fallback
        }
    };
    let Some((mtime, len)) = cache_key(path) else {
        return fallback;
    };
    if let Some(&(cm, cl, size)) = cache.borrow().get(path) {
        if cm == mtime && cl == len {
            return size;
        }
    }
    let size = match crate::thumbs::bounded_dimensions(path) {
        Ok((w, h)) => {
            SIZE_DECODES.fetch_add(1, Ordering::Relaxed);
            fit(w, h)
        }
        Err(_) => fallback,
    };
    cache
        .borrow_mut()
        .insert(path.to_path_buf(), (mtime, len, size));
    size
}

/// Bucket a tilt angle (±2.5°) to the closest 0.5° CSS class.
fn tilt_class(tilt: f64) -> String {
    let i = ((tilt * 2.0).round() as i32).clamp(-5, 5);
    if i < 0 {
        format!("mtilt_n{}", i.abs())
    } else {
        format!("mtilt_p{i}")
    }
}

/// Aluminium clothespin (9×26, like upstream): brushed horizontal gradient
/// drawn as 1px strips (no cairo gradient import needed), dark slot where
/// the rope passes through, soft outline.
fn pin_widget() -> gtk::DrawingArea {
    let pin = gtk::DrawingArea::new();
    pin.set_content_width(9);
    pin.set_content_height(26);
    pin.set_draw_func(|_, cr, _, _| {
        const SHADES: [f64; 9] = [0.60, 0.72, 0.86, 0.94, 0.96, 0.90, 0.80, 0.70, 0.62];
        // Rounded-pill clip path.
        cr.move_to(4.5, 0.0);
        cr.arc(4.5, 4.0, 4.0, -std::f64::consts::FRAC_PI_2, 0.0);
        cr.arc(4.5, 22.0, 4.0, 0.0, std::f64::consts::FRAC_PI_2);
        cr.arc(
            4.5,
            22.0,
            4.0,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI,
        );
        cr.arc(
            4.5,
            4.0,
            4.0,
            std::f64::consts::PI,
            3.0 * std::f64::consts::FRAC_PI_2,
        );
        cr.close_path();
        cr.clip();
        for (i, s) in SHADES.iter().enumerate() {
            cr.set_source_rgb(*s, *s, *s);
            cr.rectangle(i as f64, 0.0, 1.0, 26.0);
            cr.fill().ok();
        }
        // Slot the rope passes through.
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
        cr.rectangle(2.0, 8.0, 5.0, 1.6);
        cr.fill().ok();
        // Outline.
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.22);
        cr.set_line_width(0.7);
        cr.move_to(4.5, 0.0);
        cr.arc(4.5, 4.0, 4.0, -std::f64::consts::FRAC_PI_2, 0.0);
        cr.arc(4.5, 22.0, 4.0, 0.0, std::f64::consts::FRAC_PI_2);
        cr.arc(
            4.5,
            22.0,
            4.0,
            std::f64::consts::FRAC_PI_2,
            std::f64::consts::PI,
        );
        cr.arc(
            4.5,
            4.0,
            4.0,
            std::f64::consts::PI,
            3.0 * std::f64::consts::FRAC_PI_2,
        );
        cr.close_path();
        cr.stroke().ok();
    });
    pin
}

/// Phase to motion classes. Single table, so the mapping lives in one place.
fn phase_classes(frame: &CardFrame) -> Vec<String> {
    let mut classes = match frame.phase {
        Phase::Arriving => vec![String::from("marrive")],
        Phase::Gust => vec![String::from("mbreeze"), format!("d{}", frame.gust_slot)],
        Phase::Pressed => vec![String::from("mpressed")],
        Phase::Hovered => vec![String::from("mhover")],
        Phase::Rest => Vec::new(),
    };
    if frame.copy_nudge {
        classes.push(String::from("mcopy"));
    }
    classes
}

fn card_widget(
    shared: &Shared,
    frame: &CardFrame,
    path: &Path,
    badge: bool,
) -> (gtk::Widget, gtk::Overlay) {
    let id = frame.id;
    // Column: pin overlapping the card top, tilted together from the clip.
    let col = gtk::Overlay::new();
    col.add_css_class(&tilt_class(frame.tilt_deg));

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    vbox.set_margin_top(14);
    col.set_child(Some(&vbox));

    let outer = gtk::Overlay::new();
    outer.add_css_class("mansher-card");
    for class in phase_classes(frame) {
        outer.add_css_class(&class);
    }
    // Arrival is a one-shot from hang: the blank frame drops immediately and
    // the thumb swaps in mid-flight without replaying (the id stays claimed
    // in `arrived`). Waiting for the thumb first would miss the window
    // whenever a decode is slower than the arrival, and the card would pop
    // in statically instead of dropping.
    let thumb = thumbs::cached_thumb(path);
    let play_arrival = frame.phase == Phase::Arriving && shared.borrow_mut().arrived.insert(id);
    if !play_arrival {
        outer.remove_css_class("marrive");
    }
    vbox.append(&outer);

    let pin = pin_widget();
    pin.set_halign(gtk::Align::Center);
    pin.set_valign(gtk::Align::Start);
    col.add_overlay(&pin);

    let (pw, ph) = photo_display_size(&shared.borrow().size_cache, path);
    // Thumbs keep natural size == display size, so rows always fit.
    // Before its thumb lands, a fixed empty frame holds the exact layout.
    if let Some(thumb) = thumb {
        let pic = gtk::Picture::for_filename(&thumb);
        pic.set_can_shrink(false);
        pic.set_size_request(pw, ph);
        pic.set_halign(gtk::Align::Center);
        pic.set_valign(gtk::Align::Center);
        pic.add_css_class("mansher-photo");
        outer.set_child(Some(&pic));
    } else {
        let blank = gtk::Box::new(gtk::Orientation::Vertical, 0);
        blank.set_size_request(pw, ph);
        blank.set_halign(gtk::Align::Center);
        blank.set_valign(gtk::Align::Center);
        outer.set_child(Some(&blank));
    }

    // Corner cross (top-left, shows on hover) — discards like upstream.
    let cross = gtk::Button::with_label("✕");
    // Mouse-only affordance: a focusable cross would grab focus on press and
    // the ScrolledWindow would jump to reveal it (right-click scroll glitch).
    cross.set_focusable(false);
    cross.add_css_class("mansher-cross");
    cross.set_halign(gtk::Align::Start);
    cross.set_valign(gtk::Align::Start);
    // Rebuild restores the cross; no new enter arrives under a static pointer.
    cross.set_visible(shared.borrow().hovered == Some(id));
    {
        let shared = Rc::clone(shared);
        cross.connect_clicked(move |_| do_discard(&shared, id));
    }
    outer.add_overlay(&cross);

    // "Copied" pill (bottom), 1.2s like upstream.
    if badge {
        let badge = gtk::Label::new(Some("Copied ✓"));
        badge.add_css_class("mansher-badge");
        badge.set_halign(gtk::Align::Center);
        badge.set_valign(gtk::Align::End);
        outer.add_overlay(&badge);
    }

    // Hover shows the cross and arms the hover scale; the tick picks it
    // up, so entering never rebuilds mid-gesture.
    let motion = gtk::EventControllerMotion::new();
    let weak = cross.downgrade();
    {
        let shared = Rc::clone(shared);
        motion.connect_enter(move |_, _, _| {
            if let Some(c) = weak.upgrade() {
                c.set_visible(true);
            }
            shared.borrow_mut().hovered = Some(id);
        });
    }
    let weak = cross.downgrade();
    {
        let shared = Rc::clone(shared);
        motion.connect_leave(move |_| {
            // Destroyed widget keeps hovered so the rebuild restores the cross.
            if let Some(c) = weak.upgrade() {
                c.set_visible(false);
                let mut app = shared.borrow_mut();
                if app.hovered == Some(id) {
                    app.hovered = None;
                }
            }
        });
    }
    // NOTE: motion/click/drag controllers all live on the column, so the
    // pin is part of the grabbable card, exactly like upstream's GrabArea.

    col.add_controller(motion);

    // The pressed button is stored here, never re-read on release: by the
    // time ::released fires the gesture has ended its sequence, so
    // current_button() no longer reports it (upstream stores it in mouseDown
    // and reads it back in mouseUp for the same reason).
    let click = gtk::GestureClick::new();
    click.set_button(0); // all buttons; we dispatch below
    {
        let shared = Rc::clone(shared);
        let path = path.to_path_buf();
        click.connect_pressed(move |g, n_press, x, y| {
            let button = g.current_button();
            eprintln!("[DEBUG-m2] pressed button={button} id={id} x={x:.0} y={y:.0}");
            if button == 3 {
                let root = shared.borrow().popover_root.clone();
                let here = g
                    .widget()
                    .and_then(|w| w.translate_coordinates(&root, x, y));
                match here {
                    Some((rx, ry)) => {
                        eprintln!("[DEBUG-m2] menu anchor root x={rx:.0} y={ry:.0}");
                        show_menu(&shared, id, &path, &root, rx, ry);
                    }
                    None => eprintln!("[DEBUG-m2] menu translate failed"),
                }
            } else if button == 1 {
                if n_press == 2 {
                    open_path(&path);
                } else {
                    let mut app = shared.borrow_mut();
                    app.press = Some((id, button, now_ms()));
                    app.press_fired = false;
                }
            }
        });
    }
    {
        let shared = Rc::clone(shared);
        let path = path.to_path_buf();
        click.connect_released(move |_, _, _, _| {
            // Cross click destroys widgets mid-gesture; the trailing release must not copy.
            let now = now_ms();
            let mut app = shared.borrow_mut();
            if let Some((sid, t)) = app.swallow_copy {
                if sid == id && now.saturating_sub(t) < 1000 {
                    app.swallow_copy = None;
                    app.press = None;
                    return;
                }
                if now.saturating_sub(t) >= 1000 {
                    app.swallow_copy = None;
                }
            }
            let fire = match app.press {
                Some((pid, btn, t))
                    if pid == id
                        && btn == 1
                        && !app.press_fired
                        && now.saturating_sub(t) < HOLD_MS =>
                {
                    Some(pid)
                }
                _ => None,
            };
            app.press = None;
            drop(app);
            if let Some(pid) = fire {
                do_copy(&shared, pid, &path);
            }
        });
    }
    {
        let shared = Rc::clone(shared);
        click.connect_stopped(move |_| {
            shared.borrow_mut().press = None;
        });
    }
    col.add_controller(click);

    // Drag out as a real file (COPY|MOVE uri-list, like upstream's file promise).
    let drag = gtk::DragSource::new();
    drag.set_actions(gtk::gdk::DragAction::COPY | gtk::gdk::DragAction::MOVE);
    {
        let path = path.to_path_buf();
        drag.connect_prepare(move |_, _, _| {
            let uri = gtk::glib::filename_to_uri(&path, None).ok()?;
            let body = format!("{uri}\r\n").into_bytes();
            Some(gtk::gdk::ContentProvider::for_bytes(
                "text/uri-list",
                &gtk::glib::Bytes::from_owned(body),
            ))
        });
    }
    {
        let shared = Rc::clone(shared);
        drag.connect_drag_end(move |_, _, _| {
            // A move into a folder / Trash removes the file: prune it off the line.
            shared.borrow_mut().line.prune();
            refresh(&shared);
        });
    }
    {
        // The payload is only a uri-list, so GTK would show its generic file
        // icon; offer the card's own thumbnail as the drag surface instead.
        // Read through `outer` live: patch swaps the child when the thumb
        // lands, and a captured Picture would go stale.
        let outer_live = outer.clone();
        drag.connect_drag_begin(move |src, _| {
            let paintable = outer_live
                .child()
                .and_then(|c| c.downcast::<gtk::Picture>().ok())
                .and_then(|pic| pic.paintable());
            if let Some(paintable) = paintable {
                src.set_icon(Some(&paintable), 0, 0);
            }
        });
    }
    col.add_controller(drag);

    (col.upcast(), outer)
}

/// Right-click menu. Inbox files: keep/trash. External files: take down/trash.
fn show_menu(
    shared: &Shared,
    id: u128,
    path: &Path,
    anchor: &impl IsA<gtk::Widget>,
    x: f64,
    y: f64,
) {
    let inbox = shared.borrow().line.is_in_inbox_by_u128(id);
    eprintln!("[DEBUG-m2] show_menu id={id} inbox={inbox} x={x:.0} y={y:.0}");
    let pop = gtk::Popover::new();
    let vb = gtk::Box::new(gtk::Orientation::Vertical, 2);
    // Owned handles: button blocks share scope, so every `move` closure
    // clones from these instead of re-cloning a shadowed (moved) handle.
    let sh: Shared = Rc::clone(shared);
    let sh_pop: gtk::Popover = pop.clone();

    {
        let shared = Rc::clone(&sh);
        let path = path.to_path_buf();
        let b = gtk::Button::with_label("Copy");
        let pop = sh_pop.clone();
        b.connect_clicked(move |_| {
            do_copy(&shared, id, &path);
            pop.popdown();
        });
        vb.append(&b);
    }
    {
        let path = path.to_path_buf();
        let b = gtk::Button::with_label("Open");
        let pop = sh_pop.clone();
        b.connect_clicked(move |_| {
            open_path(&path);
            pop.popdown();
        });
        vb.append(&b);
    }
    {
        let path = path.to_path_buf();
        let b = gtk::Button::with_label("Annotate");
        let pop = sh_pop.clone();
        b.connect_clicked(move |_| {
            annotate_path(&path);
            pop.popdown();
        });
        vb.append(&b);
    }
    {
        let path = path.to_path_buf();
        let b = gtk::Button::with_label("Show in Files");
        let pop = sh_pop.clone();
        b.connect_clicked(move |_| {
            open_folder(&path);
            pop.popdown();
        });
        vb.append(&b);
    }
    if inbox {
        let shared = Rc::clone(&sh);
        let path = path.to_path_buf();
        let b = gtk::Button::with_label("Keep in Pictures");
        let pop = sh_pop.clone();
        b.connect_clicked(move |_| {
            match keep_in_dir(&path, &pictures_dir()) {
                Ok(_) => {
                    shared.borrow_mut().line.take_down_by_u128(id);
                    refresh(&shared);
                }
                Err(e) => eprintln!("mansher: keep failed: {e:#}"),
            }
            pop.popdown();
        });
        vb.append(&b);

        let shared = Rc::clone(&sh);
        let b = gtk::Button::with_label("Discard");
        let pop = sh_pop.clone();
        b.connect_clicked(move |_| {
            do_discard(&shared, id);
            pop.popdown();
        });
        vb.append(&b);
    } else {
        let shared = Rc::clone(&sh);
        let b = gtk::Button::with_label("Take down");
        let pop = sh_pop.clone();
        b.connect_clicked(move |_| {
            do_discard(&shared, id);
            pop.popdown();
        });
        vb.append(&b);

        let shared = Rc::clone(&sh);
        let path = path.to_path_buf();
        let b = gtk::Button::with_label("Move to Trash");
        let pop = sh_pop.clone();
        b.connect_clicked(move |_| {
            match trash_file(&path) {
                Ok(()) => {
                    let sound_on = shared.borrow().sound_on.clone();
                    shared.borrow_mut().line.drop_by_u128(id);
                    shared.borrow_mut().visuals.dropped(id, now_ms());
                    play_if_on("trash-empty", &sound_on);
                    refresh(&shared);
                }
                Err(e) => eprintln!("mansher: trash failed: {e:#}"),
            }
            pop.popdown();
        });
        vb.append(&b);
    }

    pop.set_child(Some(&vb));
    // One menu at a time: a second right-click dismisses the first instead
    // of stacking popovers. The parent is the stable root, so card rebuilds
    // can no longer orphan a live menu.
    // Bind-then-act: the borrow must end before popdown() runs. An
    // `if let ... = borrow_mut()...take()` keeps the guard alive through
    // the whole block, and popdown() emits `closed` synchronously — whose
    // hook re-borrows and aborts the process through a nounwind boundary.
    let old = shared.borrow_mut().menu_pop.take();
    if let Some(old) = old {
        old.popdown();
    }
    // Leaving the menu dismisses it: the popup lives outside the window
    // surface, so without this the line would stay revealed forever once a
    // menu opens (reaching it reads as leaving the line, and nothing ever
    // reads as leaving the menu).
    let leave_motion = gtk::EventControllerMotion::new();
    leave_motion.connect_enter(move |_, _, _| {
        eprintln!("[DEBUG-m2] menu vb-enter");
    });
    leave_motion.connect_leave(move |_| {
        eprintln!("[DEBUG-m2] menu vb-leave (tick owns dismissal now)");
    });
    vb.add_controller(leave_motion);
    // Anchor at the cursor: an unanchored popover drops at a default spot in
    // the middle of the row, far from the clicked card.
    pop.set_parent(anchor);
    pop.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    {
        let closed_shared = Rc::clone(shared);
        let closed_pop = pop.clone();
        pop.connect_closed(move |_| {
            eprintln!("[DEBUG-m2] menu closed id={id}");
            let mut app = closed_shared.borrow_mut();
            if app.menu_pop.as_ref() == Some(&closed_pop) {
                app.menu_pop.take();
            }
        });
    }
    {
        let mut app = shared.borrow_mut();
        app.menu_pop = Some(pop.clone());
        app.menu_opened_at = now_ms();
    }
    pop.popup();
    eprintln!("[DEBUG-m2] menu popup visible={}", pop.is_visible());
}

// ---------------------------------------------------------------------------
// Rope + windows
// ---------------------------------------------------------------------------

pub(crate) fn rope_widget(width: i32) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_content_width(width);
    // Tall enough that the drape midpoint (~ROPE_TOP + sag, ~61px at full
    // width) never clips: the old 50px height cut the middle of the curve
    // off, which read as the rope "fading out" in the center.
    area.set_content_height(72);
    area.set_draw_func(|_, cr, w, _h| {
        let w = w as f64;
        // One clean line, no gradient passes — reads on any background.
        // Sample the layout parabola directly instead of a Bezier with
        // extra sag offsets (those pushed the midpoint below the area).
        let y_at = |x: f64| layout::rope_y(x.max(0.0).min(w), w);
        let mut pts = Vec::with_capacity(129);
        let n = 128;
        for i in 0..=n {
            let x = -20.0 + (w + 40.0) * (i as f64) / (n as f64);
            pts.push((x, y_at(x)));
        }
        let stroke_path = || {
            cr.move_to(pts[0].0, pts[0].1);
            for (x, y) in pts.iter().skip(1) {
                cr.line_to(*x, *y);
            }
        };
        // Light halo first so the line reads on dark *and* light windows.
        // Strong halo: on dark glass the dark core vanishes, so the halo
        // carries the line; on light glass the gray core reads instead.
        cr.set_line_width(6.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
        stroke_path();
        cr.stroke().ok();
        cr.set_line_width(3.0);
        cr.set_source_rgba(0.62, 0.62, 0.65, 0.95);
        stroke_path();
        cr.stroke().ok();
    });
    area
}

pub(crate) fn apply_css() {
    // Hero look (upstream PeggedView): white glass frame, specular top edge,
    // soft shadow; photo clipped to concentric rounded corners.
    // Cards are 300 wide now; scale the cross with them.
    let cross_d = (14.0 * layout::SCALE) as i32;
    let mut css = String::from(
        "
        window.mansher { background-color: transparent; }
        .mansher-card {
            background-color: rgba(255, 255, 255, 0.78);
            border-radius: 16px;
            border-top: 1px solid rgba(255, 255, 255, 0.85);
            border-left: 1px solid rgba(255, 255, 255, 0.45);
            border-right: 1px solid rgba(255, 255, 255, 0.45);
            border-bottom: 1px solid rgba(0, 0, 0, 0.12);
            box-shadow: 0 5px 10px rgba(0, 0, 0, 0.18), 0 8px 14px rgba(0, 0, 0, 0.10);
            padding: 4px;
            transform-origin: 50% 0%;
            transition: transform 0.3s cubic-bezier(0.3, 1.6, 0.5, 1), box-shadow 0.18s ease-out, opacity 0.2s;
        }
        /* TODO: clip photo to concentric corners. GTK CSS has no `overflow`
           property, so square photo corners show inside the rounded frame.
           Options: rounded mask via snapshot vfunc, or pre-round thumbnails.
           Recorded decision (plan 010): leave until plan 011/017; not fixed here. */
        .mansher-photo {
            border-radius: 12px;
        }
        .mansher-cross {
            background-color: rgba(240, 240, 244, 0.95);
            color: #333;
            border-radius: 9999px;
            border: 1px solid rgba(0, 0, 0, 0.10);
            min-width: 20px; min-height: 20px;
            padding: 0;
        }
        .mansher-badge {
            background-color: rgba(48, 48, 52, 0.92);
            color: white;
            border-radius: 9999px;
            padding: 4px 10px;
        }
        .mansher-hint {
            background-color: rgba(48, 48, 52, 0.85);
            color: #ccc;
            border-radius: 9999px;
            padding: 6px 12px;
        }
        /* Card motion registry (upstream PeggedView/GrabArea). Tilt lives
           on the outer column, motion on the inner card node, so the two
           transforms multiply instead of overwriting each other. */
        @keyframes marrive {
            0% { transform: rotate(16deg) translateY(-46px); opacity: 0; }
            30% { transform: rotate(-2deg) translateY(3px); opacity: 1; }
            45% { transform: rotate(1.4deg) translateY(-1px); opacity: 1; }
            60% { transform: rotate(-0.7deg) translateY(0px); opacity: 1; }
            75% { transform: rotate(0.35deg) translateY(0px); opacity: 1; }
            100% { transform: rotate(0deg) translateY(0px); opacity: 1; }
        }
        .marrive {
            animation: marrive 1.1s cubic-bezier(0.2, 0.9, 0.22, 1.04) forwards;
        }
        @keyframes mbreeze {
            0% { transform: rotate(0deg); }
            25% { transform: rotate(2.4deg); }
            50% { transform: rotate(-1.2deg); }
            75% { transform: rotate(0.5deg); }
            100% { transform: rotate(0deg); }
        }
        .mbreeze {
            animation: mbreeze 0.9s ease-out;
        }
        .mbreeze.d0 { animation-delay: 0ms; }
        .mbreeze.d1 { animation-delay: 90ms; }
        .mbreeze.d2 { animation-delay: 180ms; }
        .mbreeze.d3 { animation-delay: 270ms; }
        @keyframes mcopy {
            0% { transform: rotate(0deg); }
            40% { transform: rotate(3deg); }
            100% { transform: rotate(0deg); }
        }
        .mcopy {
            animation: mcopy 0.3s ease-out;
        }
        .mhover {
            transform: scale(1.035);
            box-shadow: 0 8px 14px rgba(0, 0, 0, 0.14), 0 8px 28px rgba(0, 0, 0, 0.12);
        }
        /* 450ms matches HOLD_MS: slow press-in, release eases back on base 0.3s. */
        .mpressed {
            transition: transform 0.45s ease-in-out, box-shadow 0.18s ease-out;
            transform: scale(0.95);
        }
        /* Line reveal/tuck (upstream Line.frame): down on a 0.42s spring,
           up on a quick 220ms ease-in. Span = PANEL_HEIGHT (322) + 12. */
        @keyframes mdrop {
            from { transform: translateY(-334px); }
            to { transform: translateY(0px); }
        }
        .mreveal {
            animation: mdrop 0.42s cubic-bezier(0.2, 0.9, 0.22, 1.04);
        }
        @keyframes mtuck {
            from { transform: translateY(0px); }
            to { transform: translateY(-334px); }
        }
        .mtuck {
            animation: mtuck 0.22s ease-in forwards;
        }
    ",
    );
    css = css.replace(
        "min-width: 20px; min-height: 20px;",
        &format!("min-width: {cross_d}px; min-height: {cross_d}px;"),
    );
    // Per-card tilt buckets (±2.5° in 0.5° steps, like upstream's random tilt).
    // transform-origin top-center so cards swing from the clip, not the middle.
    for i in -5i32..=5 {
        let deg = i as f64 * 0.5;
        let name = if i < 0 {
            format!("mtilt_n{}", i.abs())
        } else {
            format!("mtilt_p{}", i)
        };
        css.push_str(&format!(
            ".{name} {{ transform: rotate({deg}deg); transform-origin: 50% 0%; }}\n"
        ));
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_data(&css);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
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

    fn test_png(w: u32, h: u32, px: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(px));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn rendered_card(id: u128, phase: Phase, badge: bool) -> RenderedCard {
        RenderedCard {
            frame: CardFrame {
                id,
                tilt_deg: 0.0,
                phase,
                gust_slot: 0,
                copy_nudge: false,
                falling: false,
            },
            path: PathBuf::from("/tmp/x.png"),
            badge,
            has_thumb: true,
        }
    }

    #[test]
    fn press_onset_detects_only_clean_press_transitions() {
        let rest = rendered_card(7, Phase::Rest, false);
        assert!(press_onset(&rest, &rendered_card(7, Phase::Pressed, false)));
        assert!(!press_onset(&rest, &rest));
        assert!(!press_onset(
            &rendered_card(7, Phase::Pressed, false),
            &rendered_card(7, Phase::Pressed, false)
        ));
        assert!(!press_onset(
            &rest,
            &rendered_card(7, Phase::Hovered, false)
        ));
        assert!(!press_onset(&rest, &rendered_card(7, Phase::Pressed, true)));
        assert!(!press_onset(
            &rest,
            &rendered_card(9, Phase::Pressed, false)
        ));
        assert!(!press_onset(
            &rendered_card(7, Phase::Pressed, false),
            &rest
        ));
    }

    #[test]
    fn size_cache_decodes_once_then_serves_stable_files() {
        let dir = temp_dir("sizecache");
        // 8x8 square scales to min(272/8, 208/8)=26 → (208, 208);
        // 16x4 wide scales to min(272/16, 208/4)=17 → (272, 68).
        let a = dir.join("a.png");
        let b = dir.join("b.png");
        std::fs::write(&a, test_png(8, 8, [10, 20, 30])).unwrap();
        std::fs::write(&b, test_png(16, 4, [200, 30, 40])).unwrap();
        let cache = RefCell::new(SizeCache::new());
        SIZE_DECODES.store(0, Ordering::SeqCst);

        assert_eq!(photo_display_size(&cache, &a), (208, 208));
        assert_eq!(SIZE_DECODES.load(Ordering::SeqCst), 1);
        assert_eq!(photo_display_size(&cache, &a), (208, 208));
        assert_eq!(SIZE_DECODES.load(Ordering::SeqCst), 1);
        assert_eq!(photo_display_size(&cache, &b), (272, 68));
        assert_eq!(SIZE_DECODES.load(Ordering::SeqCst), 2);

        // Stale entry (rewritten file under the same path) recomputes.
        cache.borrow_mut().insert(a.clone(), (0, 0, (1, 1)));
        assert_eq!(photo_display_size(&cache, &a), (208, 208));
        assert_eq!(SIZE_DECODES.load(Ordering::SeqCst), 3);

        // Missing file falls back without decoding.
        assert_eq!(
            photo_display_size(&cache, &dir.join("gone.png")),
            (layout::PHOTO_MAX_W as i32, layout::PHOTO_MAX_H as i32)
        );
        assert_eq!(SIZE_DECODES.load(Ordering::SeqCst), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
