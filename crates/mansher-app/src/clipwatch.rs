//! Clipboard screenshot capture.
//!
//! The main Print flow (grim piped to wl-copy) writes no file, so a file
//! watcher can never see it. Watch the clipboard for images instead, save
//! them to the inbox, hang them like any capture. The guard skips bytes we
//! put there ourselves on click-to-copy, or every click would hang a copy.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use mansher_core::{inbox_dir, unique_stem_path};

use crate::Msg;

pub type ClipGuard = Arc<Mutex<Option<u64>>>;

pub fn hash_bytes(b: &[u8]) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    b.hash(&mut h);
    h.finish()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn save_clip_image(png: &[u8]) -> Option<std::path::PathBuf> {
    let dir = inbox_dir().ok()?;
    save_clip_image_in(&dir, png, now_unix())
}

fn save_clip_image_in(dir: &std::path::Path, png: &[u8], now: u64) -> Option<std::path::PathBuf> {
    let stem = format!("clip-{now}-{:08x}", hash_bytes(png) & 0xffff_ffff);
    let dest = dir.join(format!("{stem}.png"));
    if dest.exists() {
        let dest = unique_stem_path(dir, &stem, "png");
        std::fs::write(&dest, png).ok()?;
        return Some(dest);
    }
    std::fs::write(&dest, png).ok()?;
    Some(dest)
}

/// Both sides hash decoded RGBA pixels: PNG roundtrips are lossless, so our
/// own click-copies compare equal and raw-file hashes never enter the loop.
fn pixels_hash(png: &[u8]) -> Option<u64> {
    let img = crate::thumbs::bounded_open_bytes(png).ok()?;
    Some(hash_bytes(img.to_rgba8().as_raw()))
}

fn handle_image(tx: &mpsc::Sender<Msg>, guard: &ClipGuard, last: &mut Option<u64>, png: Vec<u8>) {
    handle_image_in(tx, guard, last, png, save_clip_image);
}

/// Guard/dedup core with an explicit save step (tests pass a temp-dir save;
/// production passes `save_clip_image`). No behavior change.
fn handle_image_in(
    tx: &mpsc::Sender<Msg>,
    guard: &ClipGuard,
    last: &mut Option<u64>,
    png: Vec<u8>,
    save: impl Fn(&[u8]) -> Option<std::path::PathBuf>,
) {
    let Some(h) = pixels_hash(&png) else { return };
    if Some(h) == *last || Some(h) == *guard.lock().unwrap() {
        return;
    }
    *last = Some(h);
    if let Some(path) = save(&png) {
        let _ = tx.send(Msg::NewCapture(path));
    }
}

/// Event-driven on Wayland, 2s poll on X11. Silent exit when tools miss.
pub fn start(tx: mpsc::Sender<Msg>, guard: ClipGuard) {
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        start_wayland(tx, guard);
    } else {
        start_x11(tx, guard);
    }
}

/// How one watch iteration ended. Pipe close (child died or compositor
/// restarted) is restartable; a spawn failure means the tool is missing.
#[derive(Debug)]
enum WatchEnd {
    PipeClosed,
    SpawnFailed(std::io::Error),
}

const RESTART_DELAY: Duration = Duration::from_secs(5);
const BURST_WINDOW: Duration = Duration::from_secs(60);
const BURST_LIMIT: usize = 3;
const BURST_DELAY: Duration = Duration::from_secs(60);
const QUIET_RESET: Duration = Duration::from_secs(300);

/// Restart timestamps for one watcher; maps them to the next sleep so a
/// flapping child backs off instead of hot-spawning.
#[derive(Default)]
struct RestartBackoff {
    restarts: Vec<Instant>,
}

impl RestartBackoff {
    fn delay(&mut self, now: Instant) -> Duration {
        if self
            .restarts
            .last()
            .is_some_and(|t| now.duration_since(*t) > QUIET_RESET)
        {
            self.restarts.clear();
        }
        self.restarts.push(now);
        self.restarts
            .retain(|t| now.duration_since(*t) <= BURST_WINDOW);
        if self.restarts.len() >= BURST_LIMIT {
            BURST_DELAY
        } else {
            RESTART_DELAY
        }
    }
}

fn start_wayland(tx: mpsc::Sender<Msg>, guard: ClipGuard) {
    std::thread::spawn(move || {
        // `last` lives outside the loop so a restart does not re-hang the
        // pre-crash clipboard image as new.
        let mut last: Option<u64> = None;
        let mut backoff = RestartBackoff::default();
        loop {
            match watch_once(&tx, &guard, &mut last) {
                WatchEnd::SpawnFailed(e) => {
                    eprintln!("mansher: wl-paste unavailable ({e:#}), clipboard watch disabled");
                    return;
                }
                WatchEnd::PipeClosed => {
                    let wait = backoff.delay(Instant::now());
                    eprintln!(
                        "mansher: wl-paste watcher ended, restarting in {}s",
                        wait.as_secs()
                    );
                    std::thread::sleep(wait);
                }
            }
        }
    });
}

/// One watch iteration: spawn, read the pipe until EOF, reap the child.
fn watch_once(tx: &mpsc::Sender<Msg>, guard: &ClipGuard, last: &mut Option<u64>) -> WatchEnd {
    // wl-paste --watch prints one line per clipboard change to our pipe.
    let mut child = match Command::new("wl-paste")
        .arg("--watch")
        .arg("echo")
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return WatchEnd::SpawnFailed(e),
    };
    let pipe = match child.stdout.take() {
        Some(p) => p,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return WatchEnd::PipeClosed;
        }
    };
    for _line in BufReader::new(pipe).lines() {
        let types = match Command::new("wl-paste").arg("--list-types").output() {
            Ok(o) => o,
            Err(_) => continue,
        };
        if !String::from_utf8_lossy(&types.stdout)
            .lines()
            .any(|t| t.trim() == "image/png")
        {
            continue;
        }
        let data = match Command::new("wl-paste")
            .arg("--no-newline")
            .arg("--type")
            .arg("image/png")
            .output()
        {
            Ok(o) => o,
            Err(_) => continue,
        };
        if data.stdout.is_empty() {
            continue;
        }
        handle_image(tx, guard, &mut *last, data.stdout);
    }
    let _ = child.wait();
    WatchEnd::PipeClosed
}

fn start_x11(tx: mpsc::Sender<Msg>, guard: ClipGuard) {
    if Command::new("xclip").arg("-version").output().is_err() {
        return;
    }
    std::thread::spawn(move || {
        let mut last_targets = String::new();
        let mut last: Option<u64> = None;
        loop {
            std::thread::sleep(Duration::from_secs(2));
            let targets = match Command::new("xclip")
                .args(["-selection", "clipboard", "-o", "-t", "TARGETS"])
                .output()
            {
                Ok(o) => o,
                Err(_) => return,
            };
            let list = String::from_utf8_lossy(&targets.stdout).into_owned();
            if list == last_targets {
                continue;
            }
            last_targets = list.clone();
            if !list.lines().any(|t| t.trim() == "image/png") {
                continue;
            }
            let data = match Command::new("xclip")
                .args(["-selection", "clipboard", "-o", "-t", "image/png"])
                .output()
            {
                Ok(o) => o,
                Err(_) => continue,
            };
            if data.stdout.is_empty() {
                continue;
            }
            handle_image(&tx, &guard, &mut last, data.stdout);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_dir(tag: &str) -> std::path::PathBuf {
        let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("mansher-clip-{}-{n}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn png_bytes(w: u32, h: u32, px: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(px));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    #[test]
    fn pixels_hash_survives_png_roundtrip_and_sees_differences() {
        let dir = test_dir("hash");
        let raw = png_bytes(8, 8, [10, 20, 30]);
        let p = dir.join("roundtrip.png");
        std::fs::write(&p, &raw).unwrap();
        let reloaded = std::fs::read(&p).unwrap();
        let direct = pixels_hash(&raw).unwrap();
        assert_eq!(pixels_hash(&reloaded), Some(direct));
        assert_ne!(pixels_hash(&png_bytes(8, 8, [200, 30, 40])), Some(direct));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pixels_hash_rejects_corrupt_bytes() {
        assert_eq!(pixels_hash(b"not a png"), None);
        assert_eq!(pixels_hash(&[]), None);
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                if crc & 1 == 1 {
                    crc = (crc >> 1) ^ 0xedb8_8320;
                } else {
                    crc >>= 1;
                }
            }
        }
        !crc
    }

    fn huge_header_png() -> Vec<u8> {
        fn chunk(typ: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            out.extend_from_slice(typ);
            out.extend_from_slice(data);
            let mut c = Vec::from(*typ);
            c.extend_from_slice(data);
            out.extend_from_slice(&crc32(&c).to_be_bytes());
            out
        }
        let mut out = vec![137, 80, 78, 71, 13, 10, 26, 10];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&20000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        out.extend_from_slice(&chunk(b"IHDR", &ihdr));
        out.extend_from_slice(&chunk(b"IDAT", &[0x78, 0x9c, 0x03, 0, 0, 0, 0, 1]));
        out.extend_from_slice(&chunk(b"IEND", &[]));
        out
    }

    #[test]
    fn pixels_hash_rejects_oversize_header_by_cap() {
        assert_eq!(pixels_hash(&huge_header_png()), None);
        assert_eq!(
            crate::thumbs::bounded_open_bytes(&huge_header_png()),
            Err(crate::thumbs::DecodeReject::OverLimits)
        );
    }

    #[test]
    fn pixels_hash_rejects_truncated_without_panic() {
        let full = png_bytes(32, 32, [4, 5, 6]);
        let cut = full.len() / 2;
        assert_eq!(pixels_hash(&full[..cut]), None);
    }

    #[test]
    fn pixels_hash_passes_normal_1920x1080() {
        let bytes = png_bytes(1920, 1080, [11, 22, 33]);
        let img = crate::thumbs::bounded_open_bytes(&bytes).unwrap();
        assert_eq!((img.width(), img.height()), (1920, 1080));
        assert!(pixels_hash(&bytes).is_some());
    }

    #[test]
    fn handle_image_skips_own_guard_copy() {
        let dir = test_dir("guard");
        let (tx, rx) = mpsc::channel();
        let png = png_bytes(8, 8, [10, 20, 30]);
        let guard: ClipGuard = Arc::new(Mutex::new(pixels_hash(&png)));
        let mut last = None;
        handle_image_in(&tx, &guard, &mut last, png, |b| {
            let p = dir.join("clip.png");
            std::fs::write(&p, b).unwrap();
            Some(p)
        });
        assert!(rx.try_recv().is_err());
        assert_eq!(last, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_image_sends_once_per_new_image() {
        let dir = test_dir("dedup");
        let (tx, rx) = mpsc::channel();
        let guard: ClipGuard = Arc::new(Mutex::new(None));
        let mut last = None;
        let save = |b: &[u8]| {
            let p = dir.join("clip.png");
            std::fs::write(&p, b).unwrap();
            Some(p)
        };
        let png = png_bytes(8, 8, [10, 20, 30]);
        handle_image_in(&tx, &guard, &mut last, png.clone(), &save);
        match rx.try_recv() {
            Ok(Msg::NewCapture(_)) => {}
            _ => panic!("expected one NewCapture"),
        }
        handle_image_in(&tx, &guard, &mut last, png, &save);
        assert!(rx.try_recv().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_clip_image_collision_numbers_instead_of_aliasing() {
        let dir = test_dir("clip-collision");
        let png = png_bytes(8, 8, [7, 8, 9]);
        let now = 1_700_000_000u64;
        let first = save_clip_image_in(&dir, &png, now).unwrap();
        assert_eq!(
            first.file_name().unwrap(),
            format!("clip-{now}-{:08x}.png", hash_bytes(&png) & 0xffff_ffff).as_str()
        );
        let second = save_clip_image_in(&dir, &png, now).unwrap();
        assert_ne!(second, first);
        assert_eq!(
            second.file_name().unwrap(),
            format!("clip-{now}-{:08x} 2.png", hash_bytes(&png) & 0xffff_ffff).as_str()
        );
        assert_eq!(std::fs::read(&second).unwrap(), png);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backoff_sleeps_short_then_bursts_long() {
        let now = Instant::now();
        let mut backoff = RestartBackoff::default();
        assert_eq!(backoff.delay(now), Duration::from_secs(5));
        assert_eq!(backoff.delay(now), Duration::from_secs(5));
        assert_eq!(backoff.delay(now), Duration::from_secs(60));
    }

    #[test]
    fn backoff_ignores_restarts_outside_burst_window() {
        let now = Instant::now();
        let mut backoff = RestartBackoff::default();
        backoff
            .restarts
            .push(now - BURST_WINDOW - Duration::from_secs(1));
        backoff
            .restarts
            .push(now - BURST_WINDOW - Duration::from_secs(1));
        assert_eq!(backoff.delay(now), Duration::from_secs(5));
    }

    #[test]
    fn backoff_resets_counter_after_quiet_minutes() {
        let now = Instant::now();
        let mut backoff = RestartBackoff::default();
        backoff
            .restarts
            .push(now - QUIET_RESET - Duration::from_secs(1));
        backoff
            .restarts
            .push(now - QUIET_RESET - Duration::from_secs(1));
        assert_eq!(backoff.delay(now), Duration::from_secs(5));
        assert_eq!(backoff.restarts.len(), 1);
    }
}
