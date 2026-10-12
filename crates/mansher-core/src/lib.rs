//! Portable core: no GTK here, so `cargo check -p mansher-core` works on any distro.
//! Owns the clothesline model (what is hanging) and the inbox watcher
//! (notices new screenshots via inotify). Aligned to upstream Tendedero's
//! `Line.swift` / `ScreenshotWatcher.swift` / `Inbox.swift` — see
//! `docs/FEATURE_MAP.md` for the per-behavior Linux decisions.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use directories::{ProjectDirs, UserDirs};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::sync::mpsc::{self, Receiver};
use uuid::Uuid;

pub mod card_visual;
pub mod showhide;

pub use card_visual::{CardFrame, CardState, CardVisuals, Phase, UiHints};

/// Layout constants. Panel height, spacing and pin geometry match upstream
/// `Layout` (LineView.swift); the rope itself is adapted: upstream hangs
/// every card *from the curve* (SwiftUI ZStack), while our row must stay
/// straight so it can slide — so the drape is shallow and drawn down at
/// pin-grazing height instead of a deep parabola from the top.
pub mod layout {
    /// Card geometry scale, double of upstream medium.
    pub const SCALE: f64 = 2.0;
    pub const PHOTO_MAX_W: u32 = (136.0 * SCALE) as u32;
    pub const PHOTO_MAX_H: u32 = (104.0 * SCALE) as u32;
    pub const CARD_WIDTH: f64 = (150.0 * SCALE) as i32 as f64;
    pub const SPACING: f64 = CARD_WIDTH + 24.0;
    /// Window height: photo + pin/margins/hint + 8px headroom. The headroom
    /// is load-bearing: mhover scale(1.07) grows cards downward from the pin
    /// (transform-origin top), and the hint pill sits below the row — without
    /// it the hovered bottom clips against the window.
    pub const PANEL_HEIGHT: f64 = PHOTO_MAX_H as f64 + 114.0;
    /// Rope anchor height. The rope rides ABOVE the cards (not through the
    /// pin zone): the content overlay paints over the background rope, so a
    /// rope at pin height is hidden behind card bodies and only shows in the
    /// 24px gaps. Up here the full drape reads across the window, and the
    /// pins hang from it into their cards.
    pub const ROPE_TOP: f64 = 16.0;
    pub const PIN_ABOVE: f64 = 9.5;

    /// Content top so the pin slot lands mid-range: the slot sits at rope
    /// ends plus half the max sag (15px), 8px below content top.
    pub const CONTENT_TOP_OFFSET: f64 = ROPE_TOP + 15.0 - 8.0;

    /// Drape for a full-width straight row: at most 30px. The rope stays
    /// within every pin body (pins hang from it now, cards don't follow the
    /// curve). Matches upstream's 30px sag.
    pub fn sag(width: f64) -> f64 {
        (width * 0.016).min(30.0)
    }

    pub fn rope_y(x: f64, width: f64) -> f64 {
        if width <= 0.0 {
            return ROPE_TOP;
        }
        let f = x / width;
        ROPE_TOP + 4.0 * sag(width) * f * (1.0 - f)
    }

    pub fn x(index: usize, count: usize, width: f64) -> f64 {
        let total = (count.saturating_sub(1) as f64) * SPACING;
        width / 2.0 - total / 2.0 + (index as f64) * SPACING
    }

    /// Line capacity. Upstream derives `(width - 200) / spacing` (3–12)
    /// because its row fits the screen; our cards keep a fixed size and the
    /// row slides instead, so capacity is a flat count (~2.5 screens at
    /// 1080p) and the oldest only drops when truly full.
    pub const MAX_PEGS: usize = 24;
}

/// One photo hanging on the line. Mirrors Tendedero's `Pegged`.
#[derive(Debug, Clone)]
pub struct Peg {
    pub id: Uuid,
    pub path: PathBuf,
    pub added_at: DateTime<Local>,
    /// Every photo hangs a little crooked, like on a real line (±2.5°).
    pub tilt: f64,
    pub falling: bool,
}

impl Peg {
    pub fn new(path: PathBuf) -> Self {
        // Cheap tilt without a rand dependency: hash nanos into range.
        let nanos = Local::now().timestamp_subsec_nanos() as f64;
        let tilt = (nanos % 500.0) / 100.0 - 2.5;
        Self {
            id: Uuid::new_v4(),
            path,
            added_at: Local::now(),
            tilt,
            falling: false,
        }
    }

    pub fn file_name(&self) -> String {
        self.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("screenshot"))
    }
}

/// What you can do with a peg. Mirrors Tendedero's gestures:
/// click=copy, double-click=open, hold=annotate, drag-to-folder=keep,
/// drag-to-trash or cross=discard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PegAction {
    Copy,
    Open,
    Annotate,
    RevealInFolder,
    KeepInPictures,
    Discard,
    Trash,
}

/// In-memory line state. UI subscribes to this; backends execute actions.
/// Files never move because of the model — the line is only a view onto them.
#[derive(Debug, Default)]
pub struct Line {
    pegs: Vec<Peg>,
    pub max_items: usize,
    pub sound_on: bool,
}

impl Line {
    pub fn new() -> Self {
        let mut line = Self {
            pegs: Vec::new(),
            // Full sliding capacity before restore runs: restoring into
            // the old width-derived default (8) evicted most of a saved
            // line every launch, and the watcher re-hung the evicted ones
            // as "new" with noise + churn.
            max_items: layout::MAX_PEGS,
            sound_on: true,
        };
        line.restore();
        line
    }

    pub fn pegs(&self) -> &[Peg] {
        &self.pegs
    }

    pub fn live_count(&self) -> usize {
        self.pegs.iter().filter(|p| !p.falling).count()
    }

    /// Hang a new screenshot, newest last like upstream (`items.append`).
    /// A full line drops the oldest photo off the far end, quietly.
    /// Returns None if already hanging.
    pub fn hang(&mut self, path: PathBuf) -> Option<Uuid> {
        if self.pegs.iter().any(|p| p.path == path && !p.falling) {
            return None;
        }
        let peg = Peg::new(path);
        let id = peg.id;
        self.pegs.push(peg);
        while self.live_count() > self.max_items {
            if let Some(oldest) = self.pegs.iter().find(|p| !p.falling).map(|p| p.id) {
                self.drop(oldest, true);
            } else {
                break;
            }
        }
        self.save();
        Some(id)
    }

    /// Discard a peg: it leaves the line at once. The caller trashes the
    /// file first when Trash semantics apply.
    pub fn drop(&mut self, id: Uuid, quietly: bool) {
        let _ = quietly;
        if let Some(p) = self.pegs.iter_mut().find(|p| p.id == id) {
            if p.falling {
                return;
            }
            p.falling = true;
        }
        self.reap(id);
    }

    /// Remove a peg immediately.
    pub fn reap(&mut self, id: Uuid) {
        self.pegs.retain(|p| p.id != id);
        self.save();
    }

    /// Staggered take-down of everything (UI staggers 0.06s per card).
    pub fn clear_ids(&self) -> Vec<Uuid> {
        self.pegs
            .iter()
            .filter(|p| !p.falling)
            .map(|p| p.id)
            .collect()
    }

    /// Take down a peg. `delete_file` = Trash semantics; the caller moves to
    /// Trash first (portal/GIO) and only then drops the peg.
    pub fn take_down(&mut self, id: Uuid) {
        if let Some(i) = self.pegs.iter().position(|p| p.id == id) {
            self.pegs.remove(i);
        }
        self.save();
    }

    /// u128 variants so UI crates never name the `uuid` type.
    pub fn take_down_by_u128(&mut self, id: u128) {
        self.pegs.retain(|p| p.id.as_u128() != id);
        self.save();
    }

    pub fn drop_by_u128(&mut self, id: u128) {
        if let Some(pid) = self
            .pegs
            .iter()
            .find(|p| p.id.as_u128() == id)
            .map(|p| p.id)
        {
            self.drop(pid, true);
        }
    }

    pub fn reap_by_u128(&mut self, id: u128) {
        self.pegs.retain(|p| p.id.as_u128() != id);
        self.save();
    }

    pub fn is_in_inbox_by_u128(&self, id: u128) -> bool {
        let Ok(inbox) = inbox_dir() else { return false };
        self.pegs
            .iter()
            .find(|p| p.id.as_u128() == id)
            .map(|p| p.path.starts_with(&inbox))
            .unwrap_or(false)
    }

    /// Photos whose file was deleted or moved away leave the line by themselves.
    pub fn prune(&mut self) {
        let dead: Vec<Uuid> = self
            .pegs
            .iter()
            .filter(|p| !p.falling && !p.path.exists())
            .map(|p| p.id)
            .collect();
        for id in dead {
            self.drop(id, true);
        }
    }

    /// Upstream `isInInbox`: inbox files are Trashed on discard (or the folder
    /// fills with forgotten screenshots); external files only leave the line.
    pub fn is_in_inbox(&self, id: Uuid) -> bool {
        let Ok(inbox) = inbox_dir() else { return false };
        self.pegs
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.path.starts_with(&inbox))
            .unwrap_or(false)
    }

    // -- persistence (XDG state instead of UserDefaults) --

    fn state_file() -> Option<PathBuf> {
        // Test isolation: unit tests must never touch the user's real
        // state (a `cargo test` once overwrote line.json with /tmp paths,
        // wiping the restored line every launch after).
        if let Some(dir) = std::env::var_os("MANSHER_STATE_DIR") {
            return Some(PathBuf::from(dir).join("line.json"));
        }
        ProjectDirs::from("io.github.mansher", "Mansher", "Mansher").map(|d| {
            d.state_dir()
                .unwrap_or_else(|| d.data_dir())
                .join("line.json")
        })
    }

    /// Monotonic seed watermark: files older than this were either hung
    /// before (restored from line.json) or deliberately predate tracking.
    /// Kills boot storms (every accumulated screenshot re-hanging) and
    /// stops discarded photos resurrecting after a restart — the Linux
    /// analogue of upstream's launch-date cutoff.
    fn lastseed_file() -> Option<PathBuf> {
        Self::state_file().and_then(|f| f.parent().map(|p| p.join("lastseed")))
    }

    pub fn read_seed_cutoff() -> SystemTime {
        let cutoff = Self::lastseed_file()
            .and_then(|f| std::fs::read_to_string(f).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .and_then(|secs| {
                SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(secs))
            });
        cutoff.unwrap_or_else(SystemTime::now)
    }

    pub fn write_seed_cutoff(now: SystemTime) {
        if let Some(f) = Self::lastseed_file() {
            if let Some(parent) = f.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(d) = now.duration_since(SystemTime::UNIX_EPOCH) {
                let _ = std::fs::write(f, d.as_secs().to_string());
            }
        }
    }

    fn save(&self) {
        let Some(file) = Self::state_file() else {
            return;
        };
        if let Some(parent) = file.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("mansher: state dir {}: {e:#}", parent.display());
                return;
            }
        }
        let paths: Vec<&str> = self
            .pegs
            .iter()
            .filter(|p| !p.falling)
            .filter_map(|p| p.path.to_str())
            .collect();
        let body = paths.join("\n");
        if let Err(e) = std::fs::write(&file, body) {
            eprintln!("mansher: save {}: {e:#}", file.display());
        }
    }

    fn restore(&mut self) {
        let Some(file) = Self::state_file() else {
            return;
        };
        let body = match std::fs::read_to_string(&file) {
            Ok(b) => b,
            Err(e) => {
                // Missing file on first run is normal; log anything else.
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("mansher: restore {}: {e:#}", file.display());
                }
                return;
            }
        };
        for line in body.lines() {
            let p = PathBuf::from(line);
            if p.exists() {
                self.hang(p);
            }
        }
    }
}

/// The one `name`, `name 2`, … numbering loop. Callers pass pre-split
/// stem/ext so each naming scheme keeps its exact parsing; `ext` carries no
/// dot (empty when none). Unbounded except by `u32` exhaustion, absurd on a
/// real filesystem (four billion taken siblings).
pub fn unique_stem_name_occupied(stem: &str, ext: &str, occupied: impl Fn(&str) -> bool) -> String {
    let first = if ext.is_empty() {
        stem.to_owned()
    } else {
        format!("{stem}.{ext}")
    };
    if !occupied(&first) {
        return first;
    }
    let mut n = 2u32;
    loop {
        let candidate = if ext.is_empty() {
            format!("{stem} {n}")
        } else {
            format!("{stem} {n}.{ext}")
        };
        if !occupied(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Unique path from a pre-split stem/ext (same `ext` convention as above).
pub fn unique_stem_path(folder: &Path, stem: &str, ext: &str) -> PathBuf {
    folder.join(unique_stem_name_occupied(stem, ext, |c| {
        folder.join(c).exists()
    }))
}

/// Unique destination name (`name`, `name 2`, …). Mirrors `uniqueURL`.
pub fn unique_path(folder: &Path, name: &str) -> PathBuf {
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("screenshot"));
    let ext = Path::new(name)
        .extension()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    unique_stem_path(folder, &stem, &ext)
}

/// Own inbox: XDG Data (`~/.local/share/mansher/Screenshots`), NOT Pictures —
/// so the Desktop/Pictures only get what you keep. See FEATURE_MAP §3.
pub fn inbox_dir() -> Result<PathBuf> {
    let dir = ProjectDirs::from("io.github.mansher", "Mansher", "Mansher")
        .context("no home directory")?
        .data_dir()
        .join("Screenshots");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating inbox {}", dir.display()))?;
    Ok(dir)
}

/// External dirs we also watch as a safety net (see FEATURE_MAP §3):
/// `~/Pictures/Screenshots` if it exists, never created by us.
pub fn external_watch_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(user) = UserDirs::new() {
        if let Some(pics) = user.picture_dir().map(Path::to_path_buf) {
            let s = pics.join("Screenshots");
            if s.is_dir() {
                dirs.push(s);
            }
        }
        let desktop = user.home_dir().join("Desktop");
        if desktop.is_dir() {
            dirs.push(desktop);
        }
    }
    dirs
}

pub fn is_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("png")
            | Some("jpg")
            | Some("jpeg")
            | Some("webp")
            | Some("bmp")
            | Some("tif")
            | Some("tiff")
    )
}

/// On watched Desktop dirs, only real screenshot names count (Linux has no
/// macOS xattr tag): `Screenshot*`, `Capture*`, `Screenshot from*`.
fn is_desktop_candidate(path: &Path, watch_root: &Path) -> bool {
    if watch_root
        .file_name()
        .map(|n| n == "Desktop")
        .unwrap_or(false)
    {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        return name.starts_with("screenshot") || name.starts_with("capture");
    }
    true
}

// `symlink_metadata` so the type verdict never follows links. Check-then-use
// race is accepted here: worst case we refuse a file, never act on the wrong one.
fn accept_watch_path(path: &Path, watch_root: &Path) -> bool {
    let file_type = match std::fs::symlink_metadata(path) {
        Ok(m) => m.file_type(),
        Err(_) => return false,
    };
    if file_type.is_symlink() {
        return false;
    }
    let canon_path = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let canon_root = match std::fs::canonicalize(watch_root) {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !canon_path.starts_with(&canon_root) {
        return false;
    }
    is_image(path) && is_desktop_candidate(path, watch_root)
}

/// Send already-present images newer than `cutoff` (mtime order). Runs both
/// before watcher registration effects settle AND once more right after:
/// files landing mid-registration are caught by the rescan instead of lost
/// forever. Double-sends are absorbed by `hang()` dedup.
fn seed_scan(dirs: &[PathBuf], cutoff: SystemTime, tx: &mpsc::Sender<PathBuf>) {
    for dir in dirs {
        let Ok(canon_root) = std::fs::canonicalize(dir) else {
            continue;
        };
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| accept_watch_path(p, &canon_root))
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by_key(|p| {
            std::fs::symlink_metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        });
        for p in entries {
            let fresh = std::fs::symlink_metadata(&p)
                .and_then(|m| m.modified())
                .map(|t| t >= cutoff)
                .unwrap_or(false);
            if fresh {
                let _ = tx.send(p);
            }
        }
    }
}

/// Settle worker: applies the event policy OFF the notify dispatch thread —
/// 200ms delay, image + desktop-candidate filter, `file_is_settled` (120ms),
/// then forward on the public channel. Same waits as the old inline
/// callback; what changes is bursts no longer serialize the dispatcher.
/// One thread, FIFO: per-event threads could spawn dozens under a burst.
fn settle_worker(rx: mpsc::Receiver<(PathBuf, PathBuf)>, tx: mpsc::Sender<PathBuf>) {
    for (path, root) in rx {
        // Upstream's 0.2s delayed scan: let atomic writes/renames settle.
        std::thread::sleep(std::time::Duration::from_millis(200));
        if !accept_watch_path(&path, &root) {
            continue;
        }
        if file_is_settled(&path) {
            let _ = tx.send(path);
        }
    }
}

/// Watch the inbox + external dirs, yielding new image paths.
/// Debounce + settle-wait mirror upstream's 0.2s delayed scan.
pub fn watch_inbox() -> Result<(Vec<RecommendedWatcher>, Receiver<PathBuf>)> {
    let inbox = inbox_dir()?;
    let mut dirs = vec![inbox];
    dirs.extend(external_watch_dirs());

    let (tx, rx) = mpsc::channel::<PathBuf>();

    // Settle worker first: the notify callback only forwards (path, root),
    // so event bursts never block the dispatch thread. The worker owns
    // `out_tx`; `tx` is forgotten below to keep the public channel open.
    let (work_tx, work_rx) = mpsc::channel::<(PathBuf, PathBuf)>();
    let out_tx = tx.clone();
    std::thread::spawn(move || settle_worker(work_rx, out_tx));

    // Watchers BEFORE the seed scan: anything landing mid-registration is
    // caught by the rescan below instead of lost forever.
    let mut watchers = Vec::new();
    for dir in &dirs {
        let tx_work = work_tx.clone();
        let root = dir.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
            let Ok(event) = res else { return };
            if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                return;
            }
            // Non-blocking handoff: no stat, no sleep on this thread.
            for p in event.paths {
                let _ = tx_work.send((p, root.clone()));
            }
        })?;
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
        watchers.push(watcher);
    }
    drop(work_tx);

    // Seed with what is already hanging there (newest heuristic: mtime),
    // but only what is newer than the seed watermark: older files were
    // either restored from line.json already or predate tracking (same
    // cutoff rule as upstream's launch date).
    let cutoff = Line::read_seed_cutoff();
    seed_scan(&dirs, cutoff, &tx);
    Line::write_seed_cutoff(SystemTime::now());
    // Rescan with the same cutoff: files that landed during registration
    // are already evented or caught here; double-sends are absorbed by
    // hang() dedup.
    seed_scan(&dirs, cutoff, &tx);

    std::mem::forget(tx);
    Ok((watchers, rx))
}

fn file_is_settled(path: &Path) -> bool {
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let a = size(path);
    std::thread::sleep(std::time::Duration::from_millis(120));
    let b = size(path);
    a > 0 && a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    /// Cargo tests share one process: env is global, so the tests that
    /// persist state serialize on this lock, each with a private temp
    /// state dir. Without this, `save()` overwrote the user's real
    /// line.json with /tmp fixture paths.
    static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_state_dir(f: impl FnOnce()) {
        let _guard = STATE_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("mansher-state-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("MANSHER_STATE_DIR", &dir);
        f();
        std::env::remove_var("MANSHER_STATE_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hang_dedupes_and_evicts_oldest_first() {
        with_state_dir(|| {
            let mut line = Line::default();
            line.max_items = 2;
            line.hang(PathBuf::from("/tmp/a.png"));
            assert!(line.hang(PathBuf::from("/tmp/a.png")).is_none());
            line.hang(PathBuf::from("/tmp/b.png"));
            line.hang(PathBuf::from("/tmp/c.png"));
            // Oldest (a) is gone; newest stays live.
            assert_eq!(line.live_count(), 2);
            assert_eq!(line.pegs().len(), 2);
            assert!(line.pegs().iter().all(|p| !p.falling));
            assert!(line
                .pegs()
                .iter()
                .all(|p| p.path != PathBuf::from("/tmp/a.png")));
        });
    }

    #[test]
    fn layout_scale_derives_large_geometry() {
        assert!((layout::SCALE - 2.0).abs() < 1e-9);
        assert_eq!(layout::PHOTO_MAX_W, 272);
        assert_eq!(layout::PHOTO_MAX_H, 208);
        assert!((layout::CARD_WIDTH - 300.0).abs() < 1e-9);
        assert!((layout::SPACING - 324.0).abs() < 1e-9);
        assert!((layout::PANEL_HEIGHT - 322.0).abs() < 1e-9);
    }

    #[test]
    fn line_capacity_is_flat_count() {
        // No width math: cards keep a fixed size and the row slides.
        assert_eq!(layout::MAX_PEGS, 24);
        assert!((layout::sag(1920.0) - 30.0).abs() < 1e-6);
    }

    #[test]
    fn rope_droops_midspan() {
        assert!((layout::rope_y(0.0, 1920.0) - layout::ROPE_TOP).abs() < 1e-9);
        assert!((layout::rope_y(960.0, 1920.0) - (layout::ROPE_TOP + 30.0)).abs() < 1e-6);
    }

    #[test]
    fn content_top_offset_lands_pin_slot_on_rope_mid() {
        assert!((layout::CONTENT_TOP_OFFSET - (layout::ROPE_TOP + 15.0 - 8.0)).abs() < 1e-6);
        assert!((layout::CONTENT_TOP_OFFSET - 23.0).abs() < 1e-6);
    }

    #[test]
    fn unique_path_numbers_like_finder() {
        let dir = std::env::temp_dir().join(format!("mansher-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("s.png"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "s.png").file_name().unwrap(), "s 2.png");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_stem_path_first_free_returns_name() {
        let dir = std::env::temp_dir().join(format!("mansher-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_stem_path(&dir, "s", "png"), dir.join("s.png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_stem_path_no_ext_numbers_without_dot() {
        let dir = std::env::temp_dir().join(format!("mansher-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README"), b"x").unwrap();
        assert_eq!(
            unique_stem_path(&dir, "README", "").file_name().unwrap(),
            "README 2"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_path_multi_dot_keeps_full_stem() {
        let dir = std::env::temp_dir().join(format!("mansher-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.b.png"), b"x").unwrap();
        assert_eq!(
            unique_path(&dir, "a.b.png").file_name().unwrap(),
            "a.b 2.png"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_restore_roundtrip() {
        with_state_dir(|| {
            let dir = std::env::temp_dir().join(format!("mansher-rt-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let a = dir.join("a.png");
            let b = dir.join("b.png");
            std::fs::write(&a, b"a").unwrap();
            std::fs::write(&b, b"b").unwrap();
            {
                let mut line = Line::default();
                line.max_items = 24;
                line.hang(a.clone()).unwrap();
                line.hang(b.clone()).unwrap();
            }
            let mut fresh = Line::default();
            fresh.max_items = 24;
            fresh.restore();
            let paths: Vec<PathBuf> = fresh.pegs().iter().map(|p| p.path.clone()).collect();
            assert!(paths.contains(&a));
            assert!(paths.contains(&b));
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn take_down_and_prune() {
        with_state_dir(|| {
            let dir = std::env::temp_dir().join(format!("mansher-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let f = dir.join("s.png");
            std::fs::write(&f, b"fake").unwrap();

            let mut line = Line::default();
            let id = line.hang(f.clone()).unwrap();
            line.take_down(id);
            assert!(line.pegs().is_empty());

            let id = line.hang(f.clone()).unwrap();
            std::fs::remove_file(&f).unwrap();
            line.prune();
            assert!(line.pegs().iter().find(|p| p.id == id).is_none());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn seed_cutoff_missing_file_returns_now() {
        with_state_dir(|| {
            let before = SystemTime::now();
            let cutoff = Line::read_seed_cutoff();
            let after = SystemTime::now();
            assert!(cutoff >= before && cutoff <= after);
        });
    }

    #[test]
    fn seed_cutoff_corrupt_content_returns_now() {
        with_state_dir(|| {
            let f = Line::lastseed_file().unwrap();
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, "garbage").unwrap();
            let before = SystemTime::now();
            let cutoff = Line::read_seed_cutoff();
            let after = SystemTime::now();
            assert!(cutoff >= before && cutoff <= after);
        });
    }

    #[test]
    fn seed_cutoff_valid_past_epoch_returns_that_time() {
        with_state_dir(|| {
            let f = Line::lastseed_file().unwrap();
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, "1000000").unwrap();
            let want = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
            assert_eq!(Line::read_seed_cutoff(), want);
        });
    }

    #[test]
    fn seed_cutoff_far_future_value_returns_that_time() {
        with_state_dir(|| {
            let f = Line::lastseed_file().unwrap();
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, "9999999999").unwrap();
            let want = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(9_999_999_999);
            assert_eq!(Line::read_seed_cutoff(), want);
        });
    }

    #[test]
    fn file_is_settled_rejects_empty_file() {
        let dir = std::env::temp_dir().join(format!("mansher-settle-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("empty.png");
        std::fs::write(&f, b"").unwrap();
        assert!(!file_is_settled(&f));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_is_settled_accepts_stable_file() {
        let dir = std::env::temp_dir().join(format!("mansher-settle-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("stable.png");
        std::fs::write(&f, b"pixels").unwrap();
        assert!(file_is_settled(&f));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settle_worker_surfaces_files_in_order_and_filters_non_images() {
        let dir = std::env::temp_dir().join(format!("mansher-worker-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.png");
        let b = dir.join("b.png");
        let txt = dir.join("notes.txt");
        std::fs::write(&a, b"pixels-a").unwrap();
        std::fs::write(&b, b"pixels-b").unwrap();
        std::fs::write(&txt, b"not an image").unwrap();
        let (work_tx, work_rx) = mpsc::channel();
        let (out_tx, out_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || settle_worker(work_rx, out_tx));
        work_tx.send((a.clone(), dir.clone())).unwrap();
        work_tx.send((txt, dir.clone())).unwrap();
        work_tx.send((b.clone(), dir.clone())).unwrap();
        drop(work_tx);
        // FIFO, no timeouts: the worker drains, drops its sender, and the
        // stream ends with Disconnect once both images are through.
        assert_eq!(out_rx.recv().unwrap(), a);
        assert_eq!(out_rx.recv().unwrap(), b);
        assert!(out_rx.recv().is_err());
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_scan_rescan_double_send_deduped_by_hang() {
        with_state_dir(|| {
            let dir = std::env::temp_dir().join(format!("mansher-reseed-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let f = dir.join("shot.png");
            std::fs::write(&f, b"pixels").unwrap();
            let cutoff = SystemTime::now() - std::time::Duration::from_secs(60);
            let (tx, rx) = mpsc::channel();
            seed_scan(std::slice::from_ref(&dir), cutoff, &tx);
            seed_scan(std::slice::from_ref(&dir), cutoff, &tx);
            drop(tx);
            let got: Vec<PathBuf> = rx.iter().collect();
            assert_eq!(got.len(), 2);
            let mut line = Line::default();
            line.max_items = 24;
            assert!(line.hang(got[0].clone()).is_some());
            assert!(line.hang(got[1].clone()).is_none());
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    #[test]
    fn watch_accept_refuses_symlink_to_real_png_inside_root() {
        let dir = std::env::temp_dir().join(format!("mansher-watch-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real.png");
        std::fs::write(&target, b"pixels").unwrap();
        let link = dir.join("link.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!accept_watch_path(&link, &dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_accept_refuses_symlink_to_outside_file() {
        let dir = std::env::temp_dir().join(format!("mansher-watch-{}", Uuid::new_v4()));
        let outside = std::env::temp_dir().join(format!("mansher-watch-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let target = outside.join("outside.png");
        std::fs::write(&target, b"pixels").unwrap();
        let link = dir.join("link.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!accept_watch_path(&link, &dir));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn watch_accept_accepts_regular_file_inside_root() {
        let dir = std::env::temp_dir().join(format!("mansher-watch-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("shot.png");
        std::fs::write(&f, b"pixels").unwrap();
        assert!(accept_watch_path(&f, &dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_accept_refuses_dotdot_escape() {
        let base = std::env::temp_dir().join(format!("mansher-watch-{}", Uuid::new_v4()));
        let root = base.join("root");
        std::fs::create_dir_all(&root).unwrap();
        let outside = base.join("outside.png");
        std::fs::write(&outside, b"pixels").unwrap();
        let escape = root.join("../outside.png");
        assert!(!accept_watch_path(&escape, &root));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn watch_accept_refuses_dangling_symlink() {
        let dir = std::env::temp_dir().join(format!("mansher-watch-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("dangling.png");
        std::os::unix::fs::symlink(dir.join("nope.png"), &link).unwrap();
        assert!(!accept_watch_path(&link, &dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn watch_accept_desktop_prefix_decoy_refuses_link_accepts_real() {
        let base = std::env::temp_dir().join(format!("mansher-watch-{}", Uuid::new_v4()));
        let root = base.join("Desktop");
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("target.png");
        std::fs::write(&target, b"pixels").unwrap();
        let link = root.join("Screenshot Evil.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!accept_watch_path(&link, &root));
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, b"pixels").unwrap();
        assert!(accept_watch_path(&link, &root));
        let _ = std::fs::remove_dir_all(&base);
    }
}
